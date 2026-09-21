//! Local Matrix enrollment and the single-worker live driver.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agent_client_protocol::DynConnectTo;
use anyhow::{Context, Result, ensure};
use matrix_sdk::{
    AuthSession, Client, RoomState,
    authentication::matrix::MatrixSession,
    config::{SyncSettings, SyncToken},
    encryption::verification::VerificationRequestState,
};

use crate::{
    acp::ScopedProcess,
    config::Config,
    core::Bridge,
    matrix::{MatrixAdapter, build_client},
    runner::Runner,
    store::Store,
};

const SESSION_FILE: &str = "session.json";
const STORE_KEY_FILE: &str = "crypto.passphrase";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before Unix epoch")
        .as_secs() as i64
}

fn private_read(path: &Path) -> Result<String> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("missing {}", path.display()))?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "private file must be regular"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "private file permissions must be 0600"
        );
    }
    Ok(fs::read_to_string(path)?)
}

fn private_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("private path has no parent")?;
    let temp = parent.join(format!(
        ".{}.{}",
        path.file_name().unwrap().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    file.write_all(contents)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn matrix_session(client: &Client) -> Result<MatrixSession> {
    match client.session().context("Matrix session unavailable")? {
        AuthSession::Matrix(session) => Ok(session),
        _ => anyhow::bail!("this bridge currently requires a Matrix password session"),
    }
}

fn save_session(client: &Client, path: &Path) -> Result<()> {
    private_write(path, &serde_json::to_vec(&matrix_session(client)?)?)
}

fn install_session_callbacks(client: &Client, path: PathBuf) -> Result<()> {
    let read_path = path.clone();
    client.set_session_callbacks(
        Box::new(move |_| {
            let session: MatrixSession = serde_json::from_str(&private_read(&read_path)?)?;
            Ok(session.tokens)
        }),
        Box::new(move |client| {
            save_session(&client, &path)?;
            Ok(())
        }),
    )?;
    Ok(())
}

async fn restore_client(config: &Config) -> Result<Client> {
    let key = crypto_key(&config.state_dir, false)?;
    let path = config.state_dir.join(SESSION_FILE);
    let session: MatrixSession = serde_json::from_str(&private_read(&path)?)?;
    let reconciled = crate::trust::reconcile(config, &session, &key).await?;
    if reconciled > 0 {
        eprintln!("Refreshed {reconciled} room session(s) matching already-verified signing keys");
    }
    let client = build_client(config, &key).await?;
    // Install persistence before restoration can start background SDK work.
    install_session_callbacks(&client, path)?;
    client.restore_session(session).await?;
    client
        .encryption()
        .wait_for_e2ee_initialization_tasks()
        .await;
    Ok(client)
}

/// Metadata-only diagnostics using the same lock and token persistence as run.
pub async fn status(config: Config) -> Result<()> {
    let _store = Store::open(&config.state_dir, &config.bot_user_id)?;
    let client = restore_client(&config).await?;
    client
        .sync_once(SyncSettings::default().timeout(Duration::ZERO))
        .await?;
    println!("device={}", client.device_id().context("missing device")?);
    println!(
        "cross_signing={:?}",
        client.encryption().cross_signing_status().await
    );
    let users: std::collections::BTreeSet<_> = config
        .rooms
        .iter()
        .flat_map(|room| room.operators.iter().cloned())
        .chain(std::iter::once(config.bot_user_id.clone()))
        .collect();
    for user in users {
        let id: ruma::OwnedUserId = user.parse()?;
        let identity = client.encryption().request_user_identity(&id).await?;
        println!(
            "user={user} identity_verified={}",
            identity.as_ref().is_some_and(|i| i.is_verified())
        );
        for device in client.encryption().get_user_devices(&id).await?.devices() {
            println!(
                "device={} locally_or_cross_verified={} cross_verified={}",
                device.device_id(),
                device.is_verified(),
                device.is_verified_with_cross_signing()
            );
        }
    }
    for room_config in &config.rooms {
        let room_id: ruma::OwnedRoomId = room_config.room_id.parse()?;
        let room = client.get_room(&room_id).context("room missing")?;
        let mut options = matrix_sdk::room::MessagesOptions::backward();
        options.limit = ruma::uint!(20);
        for event in room.messages(options).await?.chunk {
            if let Some(incoming) = crate::matrix::decode(&room_config.room_id, &event)? {
                println!(
                    "event={} sender={} verified={} mentioned={} thread={} trust={:?}",
                    incoming.event_id,
                    incoming.sender,
                    incoming.verified_device,
                    incoming.mentions.contains(&config.bot_user_id),
                    incoming.thread_root.is_some(),
                    event.encryption_info().map(|i| &i.verification_state)
                );
            }
        }
    }
    Ok(())
}

async fn configured_event(
    client: &Client,
    config: &Config,
    event_id: &str,
) -> Result<crate::model::Incoming> {
    ensure!(
        config.rooms.len() == 1,
        "event recovery requires one configured room"
    );
    let room_id = &config.rooms[0].room_id;
    let id: ruma::OwnedRoomId = room_id.parse()?;
    let event_id: ruma::OwnedEventId = event_id.parse()?;
    let room = client.get_room(&id).context("configured room missing")?;
    let event = room.event(&event_id, None).await?;
    let incoming = crate::matrix::decode(room_id, &event)?
        .context("event is not a decryptable encrypted text message")?;
    ensure!(
        incoming.event_id == event_id.as_str(),
        "server returned a different event"
    );
    Ok(incoming)
}

/// Explicit operator diagnostic; unlike status, this prints the selected body.
pub async fn inspect_event(config: Config, event_id: &str) -> Result<()> {
    let _store = Store::open(&config.state_dir, &config.bot_user_id)?;
    let client = restore_client(&config).await?;
    let event = configured_event(&client, &config, event_id).await?;
    println!(
        "event={} sender={} verified={} body={:?}",
        event.event_id, event.sender, event.verified_device, event.body
    );
    Ok(())
}

fn crypto_key(dir: &Path, create: bool) -> Result<String> {
    let path = dir.join(STORE_KEY_FILE);
    if create && !path.exists() {
        let key = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        private_write(&path, key.as_bytes())?;
    }
    private_read(&path)
}

pub async fn enroll(config: Config) -> Result<()> {
    let _store = Store::open(&config.state_dir, &config.bot_user_id)?;
    let session_path = config.state_dir.join(SESSION_FILE);
    let existing = session_path.exists();
    let key = crypto_key(&config.state_dir, !existing)?;
    let client = build_client(&config, &key).await?;
    install_session_callbacks(&client, session_path.clone())?;
    if existing {
        let session: MatrixSession = serde_json::from_str(&private_read(&session_path)?)?;
        client.restore_session(session).await?;
    } else {
        let password = rpassword::prompt_password("Bot password (input hidden): ")?;
        ensure!(!password.is_empty(), "bot password is empty");
        client
            .matrix_auth()
            .login_username(&config.bot_user_id, &password)
            .request_refresh_token()
            .initial_device_display_name("Matrix ACP bridge")
            .await
            .context("Matrix bot login failed")?;
        drop(password);
        save_session(&client, &session_path)?;
    }
    ensure!(
        client
            .user_id()
            .is_some_and(|id| id.as_str() == config.bot_user_id),
        "logged in as unexpected Matrix user"
    );

    if let Err(error) = client
        .encryption()
        .bootstrap_cross_signing_if_needed(None)
        .await
    {
        if let Some(challenge) = error.as_uiaa_response() {
            use ruma::api::client::uiaa;
            let password =
                rpassword::prompt_password("Bot password for device identity (input hidden): ")?;
            let mut auth = uiaa::Password::new(
                uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(
                    config.bot_user_id.clone(),
                )),
                password,
            );
            auth.session = challenge.session.clone();
            client
                .encryption()
                .bootstrap_cross_signing(Some(uiaa::AuthData::Password(auth)))
                .await?;
        } else {
            return Err(error).context("bot device identity setup failed");
        }
    }

    let initial_sync = client
        .sync_once(SyncSettings::default().timeout(Duration::ZERO))
        .await?;
    for policy in &config.rooms {
        let room_id: ruma::OwnedRoomId = policy.room_id.parse()?;
        let room = client
            .get_room(&room_id)
            .with_context(|| format!("invitation not visible for {}", policy.room_id))?;
        match room.state() {
            RoomState::Invited => {
                room.join().await.context("joining configured room")?;
            }
            RoomState::Joined => {}
            _ => anyhow::bail!("bot is neither invited to nor joined in {}", policy.room_id),
        }
    }
    client
        .sync_once(
            SyncSettings::default()
                .token(SyncToken::Specific(initial_sync.next_batch))
                .timeout(Duration::ZERO),
        )
        .await?;
    let adapter = MatrixAdapter::from_client(&config, client)?;
    for policy in &config.rooms {
        let snapshot = adapter.snapshot(&policy.room_id).await?;
        ensure!(
            snapshot.joined && snapshot.encrypted,
            "configured room {} must be joined and encrypted",
            policy.room_id
        );
        ensure!(
            snapshot.members.contains(&config.bot_user_id),
            "bot absent from joined room membership"
        );
        ensure!(
            snapshot.members.is_subset(&policy.audience),
            "unexpected joined member in {}; review audience before running",
            policy.room_id
        );
        println!(
            "Enrolled {} in {}. Joined members: {}",
            config.bot_user_id,
            policy.room_id,
            snapshot.members.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    println!(
        "Device ID: {}",
        adapter.client().device_id().context("device ID missing")?
    );
    println!("Verify this device and the operator in Element before accepting prompts.");
    Ok(())
}

/// Compare SAS emojis with a human in stock Element. Nothing is trusted until
/// the local operator types MATCH after comparing both screens.
pub async fn verify(config: Config, user_id: &str) -> Result<()> {
    ensure!(
        config.rooms.iter().any(|r| r.audience.contains(user_id)),
        "user is outside configured audience"
    );
    ensure!(
        user_id != config.bot_user_id,
        "cannot verify the bot against itself"
    );
    let _store = Store::open(&config.state_dir, &config.bot_user_id)?;
    let client = restore_client(&config).await?;
    let initial_sync = client
        .sync_once(SyncSettings::default().timeout(Duration::ZERO))
        .await?;
    let mut token = initial_sync.next_batch;
    let user: ruma::OwnedUserId = user_id.parse()?;
    // Cross-user identity verification opens an encrypted DM. A first request
    // cannot be decrypted while room keys are restricted to trusted devices.
    // Address the active Element device directly with a to-device SAS request.
    client.encryption().request_user_identity(&user).await?;
    let devices: Vec<_> = client
        .encryption()
        .get_user_devices(&user)
        .await?
        .devices()
        .collect();
    ensure!(
        !devices.is_empty(),
        "no Element devices found for {user_id}"
    );
    println!("Devices for {user_id}:");
    for device in &devices {
        println!(
            "  {} ({}, verified: {})",
            device.device_id(),
            device.display_name().unwrap_or("no display name"),
            device.is_verified()
        );
    }
    let selected = if devices.len() == 1 {
        devices.into_iter().next().expect("one device")
    } else {
        print!("Enter the device ID of your active Element session: ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        devices
            .into_iter()
            .find(|device| device.device_id().as_str() == input.trim())
            .context("selected device ID is not in the displayed list")?
    };
    let selected_id = selected.device_id().to_owned();
    let request = selected.request_verification().await?;
    println!("Verification request sent to {user_id} device {selected_id}. Accept it in Element.");
    let mut sas = None;
    let mut compared = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    while tokio::time::Instant::now() < deadline {
        let response = client
            .sync_once(
                SyncSettings::default()
                    .token(SyncToken::Specific(token))
                    .timeout(Duration::from_secs(2)),
            )
            .await?;
        token = response.next_batch;
        match request.state() {
            VerificationRequestState::Ready { .. } if sas.is_none() => {
                sas = request.start_sas().await?;
            }
            VerificationRequestState::Transitioned { verification } if sas.is_none() => {
                let incoming = verification
                    .sas()
                    .context("unsupported verification method")?;
                incoming.accept().await?;
                sas = Some(incoming);
            }
            VerificationRequestState::Cancelled(_) => {
                anyhow::bail!("verification request was cancelled")
            }
            _ => {}
        }
        if let Some(flow) = &sas {
            if flow.is_cancelled() {
                anyhow::bail!("SAS verification was cancelled");
            }
            if flow.is_done() {
                ensure!(
                    client
                        .encryption()
                        .get_device(&user, &selected_id)
                        .await?
                        .is_some_and(|device| device.is_verified_with_cross_signing()),
                    "SAS finished but the selected device lacks verified cross-signing"
                );
                ensure!(
                    client
                        .encryption()
                        .request_user_identity(&user)
                        .await?
                        .is_some_and(|identity| identity.is_verified()),
                    "SAS finished but the user's signing identity is not verified"
                );
                println!("Verified {user_id} device {selected_id} through SAS.");
                return Ok(());
            }
            if flow.can_be_presented() && !compared {
                let emojis = flow.emoji().context("Element did not offer emoji SAS")?;
                println!("Compare with Element for {user_id}:");
                for item in emojis {
                    println!("{} {}", item.symbol, item.description);
                }
                println!("Type MATCH only if all seven emojis match on both screens:");
                let mut response = String::new();
                std::io::stdin().read_line(&mut response)?;
                if response.trim() == "MATCH" {
                    flow.confirm().await?;
                } else {
                    flow.mismatch().await?;
                    anyhow::bail!("SAS comparison rejected locally");
                }
                compared = true;
            }
        }
    }
    request.cancel().await?;
    anyhow::bail!("verification timed out")
}

async fn one_step(adapter: &MatrixAdapter, runner: &mut Runner) -> Result<()> {
    while runner.try_update()?.is_some() {}
    let batch = tokio::time::timeout(
        Duration::from_secs(15),
        adapter.poll(&mut runner.bridge.store),
    )
    .await
    .context("Matrix sync timed out")??;
    MatrixAdapter::ingest_batch(runner, batch, now()).await?;
    while runner.try_update()?.is_some() {}
    runner.tick(now()).await?;
    tokio::time::timeout(Duration::from_secs(15), adapter.deliver(runner))
        .await
        .context("Matrix delivery timed out")??;
    Ok(())
}

pub async fn run(config: Config, retry_event: Option<&str>) -> Result<()> {
    let mut store = Store::open(&config.state_dir, &config.bot_user_id)?;
    let recovered = store.recover(now())?;
    if recovered > 0 {
        eprintln!("{recovered} interrupted run(s) held for review");
    }
    let client = restore_client(&config).await?;
    let adapter = MatrixAdapter::from_client(&config, client)?;
    let bridge = Bridge::new(config, store)?;
    let mut runner = Runner::new(
        bridge,
        Arc::new(|h| DynConnectTo::new(ScopedProcess(h.clone()))),
    );
    if let Some(event_id) = retry_event {
        let event = configured_event(adapter.client(), &runner.bridge.config, event_id).await?;
        let snapshot = adapter.snapshot(&event.room_id).await?;
        let handled = runner.ingest(&event, &snapshot, now()).await?;
        eprintln!("Explicit event recovery: {:?}", handled.disposition);
    }
    let mut failures = 0u32;
    let mut ready = false;
    eprintln!("Connecting to Matrix; waiting for the first successful sync...");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            result = one_step(&adapter, &mut runner) => match result {
                Ok(()) => {
                    failures = 0;
                    if !ready {
                        eprintln!("Ready. Send a new verified bot mention in a configured room.");
                        ready = true;
                    }
                },
                Err(error) => {
                    failures += 1;
                    eprintln!("Matrix step failed ({failures}/5): {error:#}");
                    if failures >= 5 { break; }
                    let backoff = Duration::from_secs(1 << failures.min(4));
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => break,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                }
            }
        }
    }
    runner.shutdown(now()).await?;
    if failures >= 5 {
        anyhow::bail!(
            "Matrix driver stopped after repeated failures; staged events remain for recovery"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_file_replacement_stays_private() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("session.json");
        private_write(&path, b"first")?;
        private_write(&path, b"second")?;
        assert_eq!(private_read(&path)?, "second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }
}
