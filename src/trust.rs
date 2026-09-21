//! Refresh cached Megolm sender metadata after cross-signing verification.
//!
//! SDK 0.19.1 does not recalculate SenderUnverified on decryption. Require the
//! SDK's newly calculated SenderVerified to match the complete identity that
//! originally established the session, including the exact master key.

use std::{sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk_crypto::{
    OlmMachineBuilder,
    olm::{InboundGroupSession, SenderData},
    store::{CryptoStore, types::Changes},
    vodozemac::{Curve25519PublicKey, Ed25519PublicKey},
};
use matrix_sdk_sqlite::SqliteCryptoStore;
use ruma::DeviceKeyAlgorithm;

use crate::config::Config;

fn verified_sender(
    session: &InboundGroupSession,
    current: SenderData,
    curve_key: Option<Curve25519PublicKey>,
    signing_key: Option<Ed25519PublicKey>,
) -> Option<SenderData> {
    if session.has_been_imported() || session.forwarder_data.is_some() {
        return None;
    }
    let (SenderData::SenderUnverified(old), SenderData::SenderVerified(new)) =
        (&session.sender_data, &current)
    else {
        return None;
    };
    let session_signing_key = session
        .signing_keys()
        .get(&DeviceKeyAlgorithm::Ed25519)
        .and_then(|key| key.ed25519());
    (old == new
        && old.device_id.is_some()
        && curve_key == Some(session.sender_key())
        && signing_key.is_some()
        && signing_key == session_signing_key)
        .then_some(current)
}

/// Call only under the worker lock and before opening the network SDK client.
pub(crate) async fn reconcile(
    config: &Config,
    session: &MatrixSession,
    key: &str,
) -> Result<usize> {
    let store =
        Arc::new(SqliteCryptoStore::open(config.state_dir.join("matrix"), Some(key)).await?);
    ensure!(
        store.load_account().await?.is_some(),
        "existing crypto account missing; refusing to replace it"
    );
    let machine = OlmMachineBuilder::new(&session.meta.user_id, &session.meta.device_id)
        .with_crypto_store(store.clone())
        .build()
        .await?;
    let mut updates = Vec::new();
    for room in &config.rooms {
        let room_id: ruma::OwnedRoomId = room.room_id.parse()?;
        for mut inbound in store
            .get_inbound_group_sessions_by_room_id(&room_id)
            .await?
        {
            let SenderData::SenderUnverified(known) = &inbound.sender_data else {
                continue;
            };
            let Some(device_id) = &known.device_id else {
                continue;
            };
            let Some(device) = machine
                .get_device(&known.user_id, device_id, Some(Duration::ZERO))
                .await?
            else {
                continue;
            };
            // The SDK verifies the device signatures and owner's cross-signing
            // chain. Local device trust alone cannot satisfy this check.
            if let Some(sender) = verified_sender(
                &inbound,
                SenderData::from_device(&device),
                device.curve25519_key(),
                device.ed25519_key(),
            ) {
                inbound.sender_data = sender;
                updates.push(inbound);
            }
        }
    }
    let count = updates.len();
    if count > 0 {
        store
            .save_changes(Changes {
                inbound_group_sessions: updates,
                ..Default::default()
            })
            .await?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk_crypto::types::EventEncryptionAlgorithm;
    use matrix_sdk_crypto::{
        olm::KnownSenderData,
        vodozemac::{
            megolm::{GroupSession, SessionConfig},
            olm::Account,
        },
    };
    use ruma::{device_id, room_id, user_id};

    #[tokio::test]
    async fn refresh_requires_identical_verified_identity_and_original_device_keys() {
        let account = Account::new();
        let keys = account.identity_keys();
        let replacement = Account::new().identity_keys();
        let known = KnownSenderData {
            user_id: user_id!("@alice:example.invalid").into(),
            device_id: Some(device_id!("A").into()),
            master_key: Box::new(keys.ed25519),
        };
        let outbound = GroupSession::new(SessionConfig::version_1());
        let mut session = InboundGroupSession::new(
            keys.curve25519,
            keys.ed25519,
            room_id!("!r:example.invalid"),
            &outbound.session_key(),
            SenderData::SenderUnverified(known.clone()),
            None,
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            None,
            false,
        )
        .unwrap();
        let candidate = SenderData::SenderVerified(known.clone());
        let check = |session: &InboundGroupSession, candidate: SenderData| {
            verified_sender(
                session,
                candidate,
                Some(keys.curve25519),
                Some(keys.ed25519),
            )
        };
        assert!(check(&session, candidate.clone()).is_some());
        for rejected in [
            SenderData::unknown(),
            SenderData::SenderUnverified(known.clone()),
            SenderData::VerificationViolation(known.clone()),
        ] {
            assert!(check(&session, rejected).is_none());
        }
        for changed in [
            KnownSenderData {
                user_id: user_id!("@mallory:example.invalid").into(),
                ..known.clone()
            },
            KnownSenderData {
                device_id: Some(device_id!("B").into()),
                ..known.clone()
            },
            KnownSenderData {
                device_id: None,
                ..known.clone()
            },
            KnownSenderData {
                master_key: Box::new(replacement.ed25519),
                ..known.clone()
            },
        ] {
            assert!(check(&session, SenderData::SenderVerified(changed)).is_none());
        }
        assert!(verified_sender(&session, candidate.clone(), None, Some(keys.ed25519)).is_none());
        assert!(
            verified_sender(&session, candidate.clone(), Some(keys.curve25519), None).is_none()
        );
        assert!(
            verified_sender(
                &session,
                candidate.clone(),
                Some(replacement.curve25519),
                Some(keys.ed25519)
            )
            .is_none()
        );
        assert!(
            verified_sender(
                &session,
                candidate.clone(),
                Some(keys.curve25519),
                Some(replacement.ed25519)
            )
            .is_none()
        );

        let mut imported = InboundGroupSession::from_export(&session.export().await).unwrap();
        imported.sender_data = session.sender_data.clone();
        assert!(check(&imported, candidate.clone()).is_none());
        session.sender_data = SenderData::VerificationViolation(known);
        assert!(check(&session, candidate).is_none());
    }
}
