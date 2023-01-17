#![allow(clippy::unwrap_used)]

use super::*;
use crate::keygen::tests::rotate_idkg_dealing_encryption_keys::{
    REGISTRY_VERSION_1, REGISTRY_VERSION_2,
};
use assert_matches::assert_matches;
use ic_crypto_internal_tls::keygen::generate_tls_key_pair_der;
use ic_crypto_temp_crypto::TempCryptoBuilder;
use ic_crypto_temp_crypto::{EcdsaSubnetConfig, NodeKeysToGenerate, TempCryptoComponent};
use ic_crypto_test_utils_keygen::{add_public_key_to_registry, add_tls_cert_to_registry};
use ic_interfaces_registry::RegistryClient;
use ic_interfaces_registry_mocks::MockRegistryClient;
use ic_registry_client_fake::FakeRegistryClient;
use ic_registry_proto_data_provider::ProtoRegistryDataProvider;
use ic_types::crypto::KeyPurpose;
use ic_types::RegistryVersion;
use openssl::asn1::Asn1Time;
use rand::SeedableRng;
use rand_chacha::ChaChaRng;
use std::sync::Arc;

#[test]
fn should_collect_correctly_key_count_metrics_for_all_keys() {
    let crypto_component = TempCryptoComponent::builder()
        .with_keys(NodeKeysToGenerate::all())
        .build();
    let key_counts = crypto_component
        .collect_key_count_metrics(REGISTRY_VERSION_1)
        .expect("Failed to collect key count metrics");
    assert_eq!(5, key_counts.get_pk_registry());
    assert_eq!(5, key_counts.get_pk_local());
    assert_eq!(5, key_counts.get_sk_local());
    assert_eq!(1, key_counts.get_idkg_pks_local());
}

#[test]
fn should_collect_correctly_key_count_metrics_for_only_node_signing_key() {
    let crypto_component = TempCryptoComponent::builder()
        .with_keys(NodeKeysToGenerate::only_node_signing_key())
        .build();
    let key_counts = crypto_component
        .collect_key_count_metrics(REGISTRY_VERSION_1)
        .expect("Failed to collect key count metrics");
    assert_eq!(1, key_counts.get_pk_registry());
    assert_eq!(1, key_counts.get_pk_local());
    assert_eq!(1, key_counts.get_sk_local());
    assert_eq!(0, key_counts.get_idkg_pks_local());
}

#[test]
fn should_count_correctly_inconsistent_numbers_of_node_signing_keys() {
    let registry_data = Arc::new(ProtoRegistryDataProvider::new());
    let registry_client = Arc::new(FakeRegistryClient::new(Arc::clone(&registry_data) as Arc<_>));
    let crypto_component = TempCryptoComponent::builder()
        .with_keys(NodeKeysToGenerate::all())
        .with_registry_client_and_data(
            Arc::clone(&registry_client) as Arc<_>,
            Arc::clone(&registry_data) as Arc<_>,
        )
        .build();

    let node_signing_pk_without_corresponding_secret_key = {
        let mut nspk = crypto_component
            .current_node_public_keys()
            .expect("Failed to retrieve node public keys")
            .node_signing_public_key
            .unwrap();
        nspk.key_value[0] ^= 0xff; // flip some bits
        nspk
    };

    add_public_key_to_registry(
        node_signing_pk_without_corresponding_secret_key,
        crypto_component.get_node_id(),
        KeyPurpose::NodeSigning,
        Arc::clone(&registry_data),
        REGISTRY_VERSION_2,
    );
    registry_client.reload();

    let key_counts = crypto_component
        .collect_key_count_metrics(REGISTRY_VERSION_2)
        .expect("Failed to collect key count metrics");
    assert_eq!(5, key_counts.get_pk_registry());
    assert_eq!(5, key_counts.get_pk_local());
    assert_eq!(4, key_counts.get_sk_local());
    assert_eq!(1, key_counts.get_idkg_pks_local());
}

#[test]
fn should_count_correctly_inconsistent_numbers_of_tls_certificates() {
    let registry_data = Arc::new(ProtoRegistryDataProvider::new());
    let registry_client = Arc::new(FakeRegistryClient::new(Arc::clone(&registry_data) as Arc<_>));
    let crypto_component = TempCryptoComponent::builder()
        .with_keys(NodeKeysToGenerate::all())
        .with_registry_client_and_data(
            Arc::clone(&registry_client) as Arc<_>,
            Arc::clone(&registry_data) as Arc<_>,
        )
        .build();

    let tls_cert_without_corresponding_secret_key = {
        let mut csprng = ChaChaRng::from_seed([9u8; 32]);
        let not_after = Asn1Time::days_from_now(31).expect("unable to create Asn1Time");
        let common_name = "another_common_name";
        let (x509_cert, _key_pair) =
            generate_tls_key_pair_der(&mut csprng, common_name, &not_after)
                .expect("error generating TLS key pair");
        TlsPublicKeyCert::new_from_der(x509_cert.bytes)
            .expect("generated X509 certificate has malformed DER encoding")
            .to_proto()
    };

    add_tls_cert_to_registry(
        tls_cert_without_corresponding_secret_key,
        crypto_component.get_node_id(),
        Arc::clone(&registry_data),
        REGISTRY_VERSION_2,
    );
    registry_client.reload();

    let key_counts = crypto_component
        .collect_key_count_metrics(REGISTRY_VERSION_2)
        .expect("Failed to collect key count metrics");
    assert_eq!(5, key_counts.get_pk_registry());
    assert_eq!(5, key_counts.get_pk_local());
    assert_eq!(4, key_counts.get_sk_local());
    assert_eq!(1, key_counts.get_idkg_pks_local());
}

mod rotate_idkg_dealing_encryption_keys {
    use super::*;
    use ic_base_types::{NodeId, PrincipalId, SubnetId};
    use ic_crypto_internal_csp::keygen::utils::idkg_dealing_encryption_pk_to_proto;
    use ic_crypto_internal_threshold_sig_ecdsa::{EccCurveType, MEGaPublicKey};
    use ic_protobuf::registry::crypto::v1::PublicKey;
    use ic_registry_keys::make_crypto_node_key;
    use ic_test_utilities::FastForwardTimeSource;

    pub(crate) const REGISTRY_VERSION_1: RegistryVersion = RegistryVersion::new(1);
    pub(crate) const REGISTRY_VERSION_2: RegistryVersion = RegistryVersion::new(2);
    const NODE_ID: u64 = 42;
    const TWO_WEEKS: Duration = Duration::from_secs(2 * 7 * 24 * 60 * 60);

    #[test]
    #[should_panic(expected = "missing local IDKG public key")]
    fn should_panic_when_no_idkg_public_key_available_locally() {
        let setup = Setup::new_with_keys_to_generate(
            NodeKeysToGenerate::all_except_idkg_dealing_encryption_key(),
        );

        let _ = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_1);
    }

    #[test]
    fn should_return_current_idkg_public_key_when_other_key_in_registry() {
        let setup = Setup::new();
        let idkg_public_key_from_registry = an_idkg_dealing_encryption_public_key();
        setup.register_idkg_public_key(idkg_public_key_from_registry.clone(), REGISTRY_VERSION_2);
        let current_idkg_public_key = setup.current_local_idkg_dealing_encryption_public_key();
        assert!(!idkg_public_key_from_registry.equal_ignoring_timestamp(&current_idkg_public_key));

        let rotated_idkg_key = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_2)
            .unwrap();

        assert_eq!(current_idkg_public_key, rotated_idkg_key);
    }

    #[test]
    fn should_rotate_idkg_public_key_when_key_from_registry_does_not_have_timestamp() {
        let setup = Setup::new();
        let idkg_public_key_before_rotation =
            setup.current_local_idkg_dealing_encryption_public_key();
        let idkg_public_key_from_registry = PublicKey {
            timestamp: None,
            ..idkg_public_key_before_rotation.clone()
        };
        setup.register_idkg_public_key(idkg_public_key_from_registry.clone(), REGISTRY_VERSION_2);
        assert!(idkg_public_key_from_registry
            .equal_ignoring_timestamp(&idkg_public_key_before_rotation));

        let rotated_idkg_key = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_2)
            .unwrap();

        assert_ne!(idkg_public_key_before_rotation, rotated_idkg_key);
        assert_eq!(
            setup.current_local_idkg_dealing_encryption_public_key(),
            rotated_idkg_key
        );
    }

    #[test]
    fn should_not_rotate_key_when_last_rotation_too_recent() {
        let setup = Setup::new();
        let idkg_public_key_before_rotation =
            setup.current_local_idkg_dealing_encryption_public_key();
        let idkg_public_key_from_registry = PublicKey {
            timestamp: Some(0),
            ..idkg_public_key_before_rotation
        };
        setup
            .register_idkg_public_key(idkg_public_key_from_registry, REGISTRY_VERSION_2)
            .set_time(Time::try_from(TWO_WEEKS - Duration::from_nanos(1)).unwrap());

        let rotated_idkg_key = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_2);

        assert_matches!(
            rotated_idkg_key,
            Err(IDkgDealingEncryptionKeyRotationError::LatestLocalRotationTooRecent)
        )
    }

    #[test]
    fn should_not_rotate_key_when_node_not_on_any_subnet() {
        let setup = Setup::new_with_ecdsa_subnet_config(None);
        let idkg_public_key_before_rotation =
            setup.current_local_idkg_dealing_encryption_public_key();
        let idkg_public_key_from_registry = PublicKey {
            timestamp: Some(0),
            ..idkg_public_key_before_rotation
        };
        setup
            .register_idkg_public_key(idkg_public_key_from_registry, REGISTRY_VERSION_2)
            .set_time(Time::try_from(TWO_WEEKS + Duration::from_nanos(1)).unwrap());

        let rotated_idkg_key = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_2);

        assert_matches!(
            rotated_idkg_key,
            Err(IDkgDealingEncryptionKeyRotationError::KeyRotationNotEnabled)
        );
    }

    #[test]
    fn should_not_rotate_key_when_node_not_on_ecdsa_subnet() {
        let setup = Setup::new_with_ecdsa_subnet_config(Some(EcdsaSubnetConfig::new(
            subnet_id(),
            Some(NodeId::from(PrincipalId::new_node_test_id(182))),
            Some(TWO_WEEKS),
        )));
        let idkg_public_key_before_rotation =
            setup.current_local_idkg_dealing_encryption_public_key();
        let idkg_public_key_from_registry = PublicKey {
            timestamp: Some(0),
            ..idkg_public_key_before_rotation
        };
        setup
            .register_idkg_public_key(idkg_public_key_from_registry, REGISTRY_VERSION_2)
            .set_time(Time::try_from(TWO_WEEKS + Duration::from_nanos(1)).unwrap());

        let rotated_idkg_key = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_2);

        assert_matches!(
            rotated_idkg_key,
            Err(IDkgDealingEncryptionKeyRotationError::KeyRotationNotEnabled)
        );
    }

    #[test]
    fn should_not_rotate_key_when_key_rotation_period_not_set() {
        let setup = Setup::new_with_ecdsa_subnet_config(Some(EcdsaSubnetConfig::new(
            subnet_id(),
            Some(node_id()),
            None,
        )));
        let idkg_public_key_before_rotation =
            setup.current_local_idkg_dealing_encryption_public_key();
        let idkg_public_key_from_registry = PublicKey {
            timestamp: Some(0),
            ..idkg_public_key_before_rotation
        };
        setup
            .register_idkg_public_key(idkg_public_key_from_registry, REGISTRY_VERSION_2)
            .set_time(Time::try_from(TWO_WEEKS + Duration::from_nanos(1)).unwrap());

        let rotated_idkg_key = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_2);

        assert_matches!(
            rotated_idkg_key,
            Err(IDkgDealingEncryptionKeyRotationError::KeyRotationNotEnabled)
        );
    }

    #[test]
    fn should_not_rotate_key_when_no_ecdsa_config_exists() {
        let setup = Setup::new_with_ecdsa_subnet_config(Some(
            EcdsaSubnetConfig::new_without_ecdsa_config(subnet_id(), Some(node_id())),
        ));
        let idkg_public_key_before_rotation =
            setup.current_local_idkg_dealing_encryption_public_key();
        let idkg_public_key_from_registry = PublicKey {
            timestamp: Some(0),
            ..idkg_public_key_before_rotation
        };
        setup
            .register_idkg_public_key(idkg_public_key_from_registry, REGISTRY_VERSION_2)
            .set_time(Time::try_from(TWO_WEEKS + Duration::from_nanos(1)).unwrap());

        let rotated_idkg_key = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_2);

        assert_matches!(
            rotated_idkg_key,
            Err(IDkgDealingEncryptionKeyRotationError::KeyRotationNotEnabled)
        );
    }

    #[test]
    fn should_not_rotate_key_when_no_ecdsa_key_ids_configured() {
        let setup = Setup::new_with_ecdsa_subnet_config(Some(
            EcdsaSubnetConfig::new_without_key_ids(subnet_id(), Some(node_id()), Some(TWO_WEEKS)),
        ));
        let idkg_public_key_before_rotation =
            setup.current_local_idkg_dealing_encryption_public_key();
        let idkg_public_key_from_registry = PublicKey {
            timestamp: Some(0),
            ..idkg_public_key_before_rotation
        };
        setup
            .register_idkg_public_key(idkg_public_key_from_registry, REGISTRY_VERSION_2)
            .set_time(Time::try_from(TWO_WEEKS - Duration::from_nanos(1)).unwrap());

        let rotated_idkg_key = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_2);

        assert_matches!(
            rotated_idkg_key,
            Err(IDkgDealingEncryptionKeyRotationError::KeyRotationNotEnabled)
        );
    }

    #[test]
    fn should_rotate_idkg_public_key() {
        let setup = Setup::new();
        let idkg_public_key_before_rotation =
            setup.current_local_idkg_dealing_encryption_public_key();
        let idkg_public_key_from_registry = PublicKey {
            timestamp: Some(0),
            ..idkg_public_key_before_rotation.clone()
        };
        setup
            .register_idkg_public_key(idkg_public_key_from_registry, REGISTRY_VERSION_2)
            .set_time(Time::try_from(TWO_WEEKS + Duration::from_nanos(1)).unwrap());

        let rotated_idkg_key = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_2)
            .expect("could not rotate key");

        assert_ne!(idkg_public_key_before_rotation, rotated_idkg_key);
        assert_eq!(
            setup.current_local_idkg_dealing_encryption_public_key(),
            rotated_idkg_key
        );
    }

    #[test]
    fn should_correctly_count_multiple_idkg_dealing_encryption_keys() {
        let setup = Setup::new();
        let idkg_public_key_before_rotation =
            setup.current_local_idkg_dealing_encryption_public_key();
        let idkg_public_key_from_registry = PublicKey {
            timestamp: Some(0),
            ..idkg_public_key_before_rotation
        };
        setup
            .register_idkg_public_key(idkg_public_key_from_registry, REGISTRY_VERSION_2)
            .set_time(Time::try_from(TWO_WEEKS + Duration::from_nanos(1)).unwrap());

        let _rotated_idkg_key = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_2)
            .expect("could not rotate key");

        let key_counts = setup
            .crypto
            .collect_key_count_metrics(REGISTRY_VERSION_2)
            .expect("Failed to collect key count metrics");
        assert_eq!(1, key_counts.get_pk_registry());
        assert_eq!(1, key_counts.get_pk_local());
        assert_eq!(1, key_counts.get_sk_local());
        assert_eq!(2, key_counts.get_idkg_pks_local());
    }

    #[test]
    fn should_correctly_get_idkg_dealing_encryption_pubkeys_count_for_multiple_keys() {
        let setup = Setup::new();
        let idkg_public_key_before_rotation =
            setup.current_local_idkg_dealing_encryption_public_key();
        let idkg_public_key_from_registry = PublicKey {
            timestamp: Some(0),
            ..idkg_public_key_before_rotation
        };
        setup
            .register_idkg_public_key(idkg_public_key_from_registry, REGISTRY_VERSION_2)
            .set_time(Time::try_from(TWO_WEEKS + Duration::from_nanos(1)).unwrap());

        let _rotated_idkg_key = setup
            .crypto
            .rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_2)
            .expect("could not rotate key");

        let idkg_dealing_encryption_pubkeys_count = setup
            .crypto
            .idkg_dealing_encryption_pubkeys_count()
            .expect("Failed to get iDKG dealing encryption pubkeys count");
        assert_eq!(2, idkg_dealing_encryption_pubkeys_count);
    }

    #[test]
    fn should_return_error_when_registry_error() {
        let mock_registry_client = registry_returning(RegistryClientError::PollLockFailed {
            error: "oh no!".to_string(),
        });
        let crypto = temp_crypto_builder()
            .with_registry(Arc::new(mock_registry_client))
            .build();

        let rotated_idkg_key = crypto.rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_1);

        assert_matches!(
            rotated_idkg_key,
            Err(IDkgDealingEncryptionKeyRotationError::RegistryError(
                RegistryClientError::PollLockFailed { error }
            )) if error.contains("oh no!")
        );
    }

    #[test]
    fn should_have_rotate_idkg_dealing_encryption_keys_returning_transient_error_if_csp_fails_to_get_current_node_public_keys(
    ) {
        use crate::common::test_utils::mockall_csp::MockAllCryptoServiceProvider;
        use ic_crypto_internal_csp::api::NodePublicKeyDataError;
        use ic_interfaces::crypto::KeyManager;
        use ic_logger::replica_logger::no_op_logger;
        use ic_protobuf::registry::subnet::v1::SubnetListRecord;
        use ic_registry_keys::{make_subnet_list_record_key, make_subnet_record_key};

        let mut csp = MockAllCryptoServiceProvider::new();
        const DETAILS_STR: &str = "test";
        csp.expect_current_node_public_keys().return_const(Err(
            NodePublicKeyDataError::TransientInternalError(DETAILS_STR.to_string()),
        ));

        let ecdsa_subnet_config =
            EcdsaSubnetConfig::new(subnet_id(), Some(node_id()), Some(TWO_WEEKS));

        let registry_data = Arc::new(ProtoRegistryDataProvider::new());

        let registry_client =
            Arc::new(FakeRegistryClient::new(Arc::clone(&registry_data) as Arc<_>));

        registry_data
            .add(
                make_subnet_record_key(ecdsa_subnet_config.subnet_id).as_str(),
                REGISTRY_VERSION_1,
                Some(ecdsa_subnet_config.subnet_record),
            )
            .expect("Failed to add subnet record key");

        let subnet_list_record = SubnetListRecord {
            subnets: vec![ecdsa_subnet_config.subnet_id.get().into_vec()],
        };

        registry_data
            .add(
                make_subnet_list_record_key().as_str(),
                REGISTRY_VERSION_1,
                Some(subnet_list_record),
            )
            .expect("Failed to add subnet list record key");

        let crypto_component = CryptoComponentFatClient::new_with_csp_and_fake_node_id(
            csp,
            no_op_logger(),
            registry_client.clone(),
            node_id(),
        );
        registry_client.reload();

        let result = crypto_component.rotate_idkg_dealing_encryption_keys(REGISTRY_VERSION_1);

        assert_matches!(result, Err(IDkgDealingEncryptionKeyRotationError::TransientInternalError(details)) if details == DETAILS_STR);
    }

    struct Setup {
        registry_data: Arc<ProtoRegistryDataProvider>,
        registry_client: Arc<FakeRegistryClient>,
        time_source: Arc<FastForwardTimeSource>,
        crypto: TempCryptoComponent,
    }

    impl Setup {
        fn new() -> Self {
            Self::new_with_ecdsa_subnet_config(Some(EcdsaSubnetConfig::new(
                subnet_id(),
                Some(node_id()),
                Some(TWO_WEEKS),
            )))
        }

        fn new_with_keys_to_generate(node_keys_to_generate: NodeKeysToGenerate) -> Self {
            Self::new_internal(
                node_keys_to_generate,
                Some(EcdsaSubnetConfig::new(
                    subnet_id(),
                    Some(node_id()),
                    Some(TWO_WEEKS),
                )),
            )
        }

        fn new_with_ecdsa_subnet_config(ecdsa_subnet_config: Option<EcdsaSubnetConfig>) -> Self {
            Self::new_internal(
                NodeKeysToGenerate::only_idkg_dealing_encryption_key(),
                ecdsa_subnet_config,
            )
        }

        fn new_internal(
            node_keys_to_generate: NodeKeysToGenerate,
            ecdsa_subnet_config: Option<EcdsaSubnetConfig>,
        ) -> Self {
            let registry_data = Arc::new(ProtoRegistryDataProvider::new());
            let registry_client =
                Arc::new(FakeRegistryClient::new(Arc::clone(&registry_data) as Arc<_>));
            let time_source = FastForwardTimeSource::new();
            let mut crypto_builder = temp_crypto_builder()
                .with_keys(node_keys_to_generate)
                .with_registry_client_and_data(
                    Arc::clone(&registry_client) as Arc<_>,
                    Arc::clone(&registry_data) as Arc<_>,
                )
                .with_time_source(Arc::clone(&time_source) as Arc<_>);
            if let Some(ecdsa_subnet_config) = ecdsa_subnet_config {
                crypto_builder = crypto_builder.with_ecdsa_subnet_config(ecdsa_subnet_config);
            }

            let setup = Setup {
                registry_data: Arc::clone(&registry_data) as Arc<_>,
                registry_client: Arc::clone(&registry_client) as Arc<_>,
                time_source: Arc::clone(&time_source) as Arc<_>,
                crypto: crypto_builder.build(),
            };
            registry_client.reload();
            setup
        }

        fn register_idkg_public_key(
            &self,
            idkg_public_key: PublicKey,
            version: RegistryVersion,
        ) -> &Self {
            let _ = &self
                .registry_data
                .add(
                    &make_crypto_node_key(node_id(), KeyPurpose::IDkgMEGaEncryption),
                    version,
                    Some(idkg_public_key),
                )
                .unwrap();
            let _ = &self.registry_client.update_to_latest_version();
            self
        }

        fn set_time(&self, time: Time) -> &Self {
            let _ = &self.time_source.set_time(time).unwrap();
            self
        }

        fn current_local_idkg_dealing_encryption_public_key(&self) -> PublicKey {
            self.crypto
                .current_node_public_keys()
                .expect("Failed to retrieve node public keys")
                .idkg_dealing_encryption_public_key
                .unwrap()
        }
    }

    fn node_id() -> NodeId {
        NodeId::from(PrincipalId::new_node_test_id(NODE_ID))
    }

    fn subnet_id() -> SubnetId {
        SubnetId::new(PrincipalId::new(29, [0xfc; 29]))
    }

    fn an_idkg_dealing_encryption_public_key() -> PublicKey {
        idkg_dealing_encryption_pk_to_proto(
            MEGaPublicKey::deserialize(
                EccCurveType::K256,
                &hex::decode("039a6f8ffe8e8d252f0ba25230a77ed334da43a8661480c07e85db88dde355f096")
                    .expect("invalid hex string"),
            )
            .unwrap(),
        )
    }

    fn registry_returning(error: RegistryClientError) -> impl RegistryClient {
        let mut registry = MockRegistryClient::new();
        registry
            .expect_get_value()
            .returning(move |_, _| Err(error.clone()));
        registry
    }

    fn temp_crypto_builder() -> TempCryptoBuilder {
        TempCryptoComponent::builder()
            .with_keys(NodeKeysToGenerate::only_idkg_dealing_encryption_key())
            // callers of rotate_idkg_dealing_encryption_keys use a CryptoComponent with a remote vault
            .with_remote_vault()
            .with_node_id(node_id())
    }
}

mod idkg_dealing_encryption_pubkeys_count {
    use super::*;
    use ic_base_types::{NodeId, PrincipalId};

    #[test]
    fn should_correctly_count_idkg_dealing_encryption_pubkeys_when_all_keys_present() {
        let crypto_component = TempCryptoComponent::builder()
            .with_keys(NodeKeysToGenerate::all())
            .build();
        let key_counts = crypto_component
            .idkg_dealing_encryption_pubkeys_count()
            .expect("Error calling idkg_dealing_encryption_pubkeys_count");
        assert_eq!(1, key_counts);
    }

    #[test]
    fn should_correctly_count_idkg_dealing_encryption_pubkeys_when_no_keys_present() {
        let crypto_component = TempCryptoComponent::builder()
            .with_keys(NodeKeysToGenerate::none())
            .build();
        let key_counts = crypto_component
            .idkg_dealing_encryption_pubkeys_count()
            .expect("Error calling idkg_dealing_encryption_pubkeys_count");
        assert_eq!(0, key_counts);
    }

    #[test]
    fn should_have_idkg_dealing_encryption_pubkeys_count_returning_transient_error_if_csp_call_fails(
    ) {
        use crate::common::test_utils::mockall_csp::MockAllCryptoServiceProvider;
        use ic_crypto_internal_csp::api::NodePublicKeyDataError;
        use ic_interfaces::crypto::KeyManager;
        use ic_logger::replica_logger::no_op_logger;

        let mut csp = MockAllCryptoServiceProvider::new();
        const DETAILS_STR: &str = "test";
        csp.expect_idkg_dealing_encryption_pubkeys_count()
            .return_const(Err(NodePublicKeyDataError::TransientInternalError(
                DETAILS_STR.to_string(),
            )));

        let registry_data = Arc::new(ProtoRegistryDataProvider::new());

        let registry_client =
            Arc::new(FakeRegistryClient::new(Arc::clone(&registry_data) as Arc<_>));

        let crypto_component = CryptoComponentFatClient::new_with_csp_and_fake_node_id(
            csp,
            no_op_logger(),
            registry_client.clone(),
            NodeId::from(PrincipalId::new_node_test_id(42)),
        );
        registry_client.reload();

        let result = crypto_component.idkg_dealing_encryption_pubkeys_count();

        assert_matches!(result, Err(IdkgDealingEncPubKeysCountError::TransientInternalError(details)) if details == DETAILS_STR);
    }
}
