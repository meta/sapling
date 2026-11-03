/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

pub mod security;

use std::iter::once;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::Error;
use anyhow::Result;
use anyhow::anyhow;
use clientinfo::ClientInfo;
use clientinfo::ClientRequestInfo;
use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;
use permission_checker::ClientCategory;
use permission_checker::MononokeIdentitySet;
use permission_checker::MononokeIdentitySetExt;
use permission_checker::TenantInfo;
use session_id::SessionId;
use session_id::generate_session_id;
use tokio::time::timeout;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ClientPathAclCompatibility {
    #[default]
    Absent,
    ReadyV1,
    Malformed,
}

impl ClientPathAclCompatibility {
    /// Classifies the value of the `scs_path_acl_compatible` request header.
    pub fn from_header_value(value: Option<&[u8]>) -> Self {
        match value {
            None => Self::Absent,
            Some(b"1") => Self::ReadyV1,
            Some(_) => Self::Malformed,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::ReadyV1 => "ready_v1",
            Self::Malformed => "malformed",
        }
    }
}

/// RIM attribution with a fixed workload/instance hierarchy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RimTenancyPath {
    region: Option<String>,
    category: ClientCategory,
    allocation: Option<String>,
    workload: String,
    instance: String,
}

impl RimTenancyPath {
    pub fn category(&self) -> ClientCategory {
        self.category
    }

    pub fn with_workload_allocation(mut self, is_named: impl FnOnce(&str, &str) -> bool) -> Self {
        self.allocation = Some(if is_named(self.category.as_str(), &self.workload) {
            self.workload.clone()
        } else {
            "__mononoke_remaining__".to_owned()
        });
        self
    }

    /// Convert to the ordered components expected by the RIM client.
    pub fn into_vec(self) -> Vec<String> {
        once("root".to_owned())
            .chain(self.region)
            .chain(once(self.category.as_str().to_owned()))
            .chain(self.allocation)
            .chain([self.workload, self.instance])
            .collect()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Metadata {
    session_id: SessionId,
    identities: MononokeIdentitySet,
    /// If the identities were proxied, this is the true and original
    /// identities from the request.
    original_identities: Option<MononokeIdentitySet>,
    /// Signer (and, for delegated tokens, certified) identities from CATs a caller
    /// forwarded on behalf of its own caller. Unverified; never used for authorization.
    unverified_forwarded_identities: Option<MononokeIdentitySet>,
    forwarded_cats_verifier: Option<String>,
    forwarded_cats_token_verifiers: Option<MononokeIdentitySet>,
    client_debug: bool,
    /// "true" if client connects from untrusted environment.
    /// We're going to apply restrictions in this case, like rejecting pushes
    /// or admin bypass.
    client_untrusted: bool,
    client_ip: Option<IpAddr>,
    client_port: Option<u16>,
    client_hostname: Option<String>,
    revproxy_region: Option<String>,
    raw_encoded_cats: Option<String>,
    client_info: Option<ClientInfo>,
    fetch_cause: Option<String>,
    fetch_from_cas_attempted: bool,
    upstream_client_id: Option<String>,
    user_agent: Option<String>,
    client_path_acl_compatibility: ClientPathAclCompatibility,
    partial_responses_opt_in: bool,
    partial_responses_opt_out: bool,
}

impl Metadata {
    pub async fn new(
        session_id: Option<&String>,
        identities: MononokeIdentitySet,
        client_debug: bool,
        client_untrusted: bool,
        client_ip: Option<IpAddr>,
        client_port: Option<u16>,
    ) -> Self {
        let session_id: SessionId = match session_id {
            Some(id) => SessionId::from_string(id.to_owned()),
            None => generate_session_id(),
        };

        // Hostname of the client is for non-critical use only. We're doing best-effort lookup here:
        // 1) We're extracting it from identities (which requires no remote calls)
        let client_hostname = if let Some(client_hostname) = identities.hostname() {
            Some(client_hostname.to_string())
        }
        // 2) If it's not there we're trying to look it up via reverse dns with timeout of 1s.
        else if let Some(client_ip) = client_ip {
            timeout(Duration::from_secs(1), Metadata::reverse_lookup(client_ip))
                .await
                .map_err(Error::from)
                .flatten()
                .ok()
        } else {
            None
        };

        Self {
            session_id,
            identities,
            original_identities: None,
            unverified_forwarded_identities: None,
            forwarded_cats_verifier: None,
            forwarded_cats_token_verifiers: None,
            client_debug,
            client_untrusted,
            client_ip,
            client_port,
            client_hostname,
            revproxy_region: None,
            raw_encoded_cats: None,
            client_info: None,
            fetch_cause: None,
            fetch_from_cas_attempted: false,
            upstream_client_id: None,
            user_agent: None,
            client_path_acl_compatibility: ClientPathAclCompatibility::Absent,
            partial_responses_opt_in: false,
            partial_responses_opt_out: false,
        }
    }

    // Reverse lookups an IP to associated hostname. Trailing dots are stripped
    // to remain compatible with historical logging and common usage of reverse
    // hostnames in other logs (even though trailing dot is technically more correct)
    async fn reverse_lookup(client_ip: IpAddr) -> Result<String> {
        // This parses /etc/resolv.conf on each request. Given that this should be in
        // the page cache and the parsing of the text is very minimal, this shouldn't
        // impact performance much. In case this does lead to performance issues we
        // could start caching this, which for now would be preferred to avoid as this
        // might lead to unexpected behavior if the system configuration changes.
        let resolver = TokioResolver::builder_tokio()?.build()?;
        let lookup = resolver.reverse_lookup(client_ip).await?;
        lookup
            .answers()
            .iter()
            .find_map(|record| {
                if let RData::PTR(ptr) = &record.data {
                    Some(ptr.0.to_string().trim_end_matches('.').to_string())
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow!("failed to do reverse lookup"))
    }

    pub fn add_raw_encoded_cats(&mut self, raw_encoded_cats: String) -> &mut Self {
        self.raw_encoded_cats = Some(raw_encoded_cats);
        self
    }

    pub fn add_revproxy_region(&mut self, revproxy_region: String) -> &mut Self {
        self.revproxy_region = Some(revproxy_region);
        self
    }

    pub fn add_client_info(&mut self, client_info: ClientInfo) -> &mut Self {
        self.client_info = Some(client_info);
        self.set_main_id()
    }

    pub fn client_info(&self) -> Option<&ClientInfo> {
        self.client_info.as_ref()
    }

    pub fn set_main_id(&mut self) -> &mut Self {
        self.client_info.as_mut().map(|x| {
            x.request_info.as_mut().map(|client_request_info| {
                if !client_request_info.has_main_id() {
                    client_request_info.set_main_id(
                        self.identities
                            .main_client_identity(x.fb.sandcastle_alias()),
                    )
                }
            })
        });
        self
    }

    pub fn add_original_identities(&mut self, identities: MononokeIdentitySet) -> &mut Self {
        self.original_identities = Some(identities);
        self
    }

    pub fn add_unverified_forwarded_identities(
        &mut self,
        verifier: String,
        identities: MononokeIdentitySet,
        token_verifiers: MononokeIdentitySet,
    ) -> &mut Self {
        self.forwarded_cats_verifier = Some(verifier);
        self.unverified_forwarded_identities = Some(identities);
        self.forwarded_cats_token_verifiers = Some(token_verifiers);
        self
    }

    pub fn update_client_untrusted(&mut self, client_untrusted: bool) -> &mut Self {
        // Be conservative: if client was already untrusted, don't allow to make
        // it trusted
        self.client_untrusted |= client_untrusted;
        self
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn identities(&self) -> &MononokeIdentitySet {
        &self.identities
    }

    pub fn likely_an_agent(&self) -> bool {
        self.identities.likely_an_agent()
    }

    pub fn original_identities(&self) -> Option<&MononokeIdentitySet> {
        self.original_identities.as_ref()
    }

    pub fn unverified_forwarded_identities(&self) -> Option<&MononokeIdentitySet> {
        self.unverified_forwarded_identities.as_ref()
    }

    pub fn forwarded_cats_verifier(&self) -> Option<&str> {
        self.forwarded_cats_verifier.as_deref()
    }

    pub fn forwarded_cats_token_verifiers(&self) -> Option<&MononokeIdentitySet> {
        self.forwarded_cats_token_verifiers.as_ref()
    }

    pub fn raw_encoded_cats(&self) -> &Option<String> {
        &self.raw_encoded_cats
    }

    pub fn set_identities(mut self, identities: MononokeIdentitySet) -> Self {
        self.identities = identities;
        self
    }

    pub fn revproxy_region(&self) -> &Option<String> {
        &self.revproxy_region
    }

    pub fn client_debug(&self) -> bool {
        self.client_debug
    }

    pub fn client_untrusted(&self) -> bool {
        self.client_untrusted
    }

    pub fn client_ip(&self) -> Option<&IpAddr> {
        self.client_ip.as_ref()
    }

    pub fn client_port(&self) -> Option<u16> {
        self.client_port
    }

    pub fn set_client_ip(mut self, client_ip: Option<IpAddr>) -> Self {
        self.client_ip = client_ip;
        self
    }

    pub fn set_client_port(mut self, client_port: Option<u16>) -> Self {
        self.client_port = client_port;
        self
    }

    pub fn client_hostname(&self) -> Option<&str> {
        self.client_hostname.as_deref()
    }

    pub fn set_client_hostname(mut self, client_hostname: Option<String>) -> Self {
        self.client_hostname = client_hostname;
        self
    }

    pub fn set_fetch_cause(mut self, fetch_cause: Option<String>) -> Self {
        self.fetch_cause = fetch_cause;
        self
    }

    pub fn set_fetch_from_cas_attempted(mut self, fetch_from_cas_attempted: bool) -> Self {
        self.fetch_from_cas_attempted = fetch_from_cas_attempted;
        self
    }

    pub fn unix_name(&self) -> Option<&str> {
        for identity in self.identities() {
            if identity.id_type() == "USER" {
                // The identity that's all numeric is likely an FBID, not a unixname
                if identity.id_data().chars().all(|c| c.is_numeric()) {
                    continue;
                }
                return Some(identity.id_data());
            }
        }

        None
    }

    pub fn sandcastle_alias(&self) -> Option<&str> {
        self.client_info
            .as_ref()
            .and_then(|ci| ci.fb.sandcastle_alias())
    }

    pub fn ci_purpose(&self) -> Option<&str> {
        self.client_info.as_ref().and_then(|ci| ci.fb.ci_purpose())
    }

    pub fn sandcastle_nonce(&self) -> Option<&str> {
        self.client_info
            .as_ref()
            .and_then(|ci| ci.fb.sandcastle_nonce())
    }

    pub fn sandcastle_vcs(&self) -> Option<&str> {
        self.client_info
            .as_ref()
            .and_then(|ci| ci.fb.sandcastle_vcs())
    }

    pub fn client_request_info(&self) -> Option<&ClientRequestInfo> {
        self.client_info
            .as_ref()
            .and_then(|ci| ci.request_info.as_ref())
    }

    pub fn tenant_info(&self) -> TenantInfo {
        let client_id = self
            .client_request_info()
            .and_then(|cri| cri.main_id.clone());

        match self.identities.client_category(self.sandcastle_alias()) {
            ClientCategory::HealthCheck => TenantInfo::HealthCheck { client_id },
            ClientCategory::InteractiveDev => TenantInfo::InteractiveDev { client_id },
            ClientCategory::DevEnv => TenantInfo::DevEnv { client_id },
            ClientCategory::CiSandcastle => TenantInfo::CiSandcastle {
                client_id,
                ci_purpose: self.ci_purpose().map(str::to_owned),
            },
            ClientCategory::SandcastleAutomation => TenantInfo::SandcastleAutomation { client_id },
            ClientCategory::Mast => TenantInfo::Mast { client_id },
            ClientCategory::FaaS => TenantInfo::FaaS {
                client_id,
                atlas_env_id: self.clientinfo_atlas_env_id().map(str::to_owned),
                atlas_rl: self.clientinfo_atlas_rl(),
                atlas_purpose: self.clientinfo_atlas_purpose().map(str::to_owned),
                faas_job_name: self.clientinfo_faas_job_name().map(str::to_owned),
            },
            ClientCategory::Automation => TenantInfo::Automation { client_id },
            ClientCategory::Unknown => TenantInfo::Unknown { client_id },
        }
    }

    /// RIM workload/instance attribution, derived independently of request `main_id`.
    /// An optional serving region groups categories under that region.
    pub fn rim_tenancy_path(&self, region: Option<&str>) -> RimTenancyPath {
        let category = self.identities.client_category(self.sandcastle_alias());
        let identity = |types: &[&str]| {
            types
                .iter()
                .find_map(|ty| self.identities.identity_type_filtered_concat(ty))
        };
        let mut workload = match category {
            ClientCategory::DevEnv => self
                .identities
                .on_demand_type()
                .or_else(|| {
                    self.identities.iter().find_map(|id| {
                        if id.id_type() != "SERVICE_IDENTITY" {
                            return None;
                        }
                        id.id_data().strip_prefix("ondemand_").filter(|ty| {
                            !matches!(*ty, "" | "intask_service" | "session" | "worker")
                        })
                    })
                })
                .map(str::to_owned),
            ClientCategory::InteractiveDev => identity(&["USER"]),
            ClientCategory::CiSandcastle => self
                .ci_purpose()
                .filter(|purpose| !purpose.is_empty())
                .map(|purpose| format!("CI_PURPOSE:{purpose}")),
            ClientCategory::Mast => identity(&["DATA_PROJECT"]),
            ClientCategory::FaaS => identity(&["CREWMATE", "ASYNC_JOB_ID"]),
            ClientCategory::HealthCheck
            | ClientCategory::SandcastleAutomation
            | ClientCategory::Automation
            | ClientCategory::Unknown => None,
        }
        .or_else(|| {
            identity(&[
                "SERVICE_IDENTITY",
                "ROOT_MACHINE_TIER",
                "CORP_SERVICE",
                "MACHINE_TIER",
                "MACHINE",
            ])
        })
        .filter(|workload| !workload.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
        if category != ClientCategory::CiSandcastle
            && let Some(cause @ ("sl-prefetch" | "edenfs-prefetch" | "eden-walk-prefetch")) =
                self.fetch_cause()
        {
            workload.push(':');
            workload.push_str(cause);
        }
        // Every category must have the same depth; a single default child adds no sharing.
        let instance = if category == ClientCategory::DevEnv {
            self.identities
                .hostname()
                .filter(|hostname| !hostname.is_empty())
                .unwrap_or("unknown")
        } else {
            "default"
        };
        RimTenancyPath {
            region: region.map(str::to_owned),
            category,
            allocation: None,
            workload,
            instance: instance.to_owned(),
        }
    }

    pub fn clientinfo_tw_job(&self) -> Option<&str> {
        self.client_info.as_ref().and_then(|ci| ci.fb.tw_job())
    }

    pub fn clientinfo_tw_task(&self) -> Option<&str> {
        self.client_info.as_ref().and_then(|ci| ci.fb.tw_task())
    }

    pub fn clientinfo_atlas(&self) -> Option<bool> {
        self.client_info.as_ref().and_then(|ci| ci.fb.is_atlas())
    }

    pub fn clientinfo_atlas_rl(&self) -> Option<bool> {
        self.client_info.as_ref().and_then(|ci| ci.fb.is_atlas_rl())
    }

    pub fn clientinfo_atlas_purpose(&self) -> Option<&str> {
        self.client_info
            .as_ref()
            .and_then(|ci| ci.fb.atlas_purpose())
    }

    pub fn clientinfo_atlas_env_id(&self) -> Option<&str> {
        self.client_info
            .as_ref()
            .and_then(|ci| ci.fb.atlas_env_id())
    }

    pub fn clientinfo_faas_job_name(&self) -> Option<&str> {
        self.client_info
            .as_ref()
            .and_then(|ci| ci.fb.faas_job_name())
    }

    pub fn fetch_cause(&self) -> Option<&str> {
        self.fetch_cause.as_deref()
    }

    pub fn fetch_from_cas_attempted(&self) -> bool {
        self.fetch_from_cas_attempted
    }

    pub fn add_upstream_client_id(&mut self, client_id: String) -> &mut Self {
        self.upstream_client_id = Some(client_id);
        self
    }

    pub fn upstream_client_id(&self) -> Option<&str> {
        self.upstream_client_id.as_deref()
    }

    pub fn set_user_agent(mut self, user_agent: Option<String>) -> Self {
        self.user_agent = user_agent;
        self
    }

    pub fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    pub fn add_client_path_acl_compatibility(
        &mut self,
        client_path_acl_compatibility: ClientPathAclCompatibility,
    ) -> &mut Self {
        self.client_path_acl_compatibility = client_path_acl_compatibility;
        self
    }

    pub fn client_path_acl_compatibility(&self) -> ClientPathAclCompatibility {
        self.client_path_acl_compatibility
    }

    pub fn add_partial_responses_opt_in(&mut self, partial_responses_opt_in: bool) -> &mut Self {
        self.partial_responses_opt_in = partial_responses_opt_in;
        self
    }

    pub fn partial_responses_opt_in(&self) -> bool {
        self.partial_responses_opt_in
    }

    pub fn add_partial_responses_opt_out(&mut self, partial_responses_opt_out: bool) -> &mut Self {
        self.partial_responses_opt_out = partial_responses_opt_out;
        self
    }

    pub fn partial_responses_opt_out(&self) -> bool {
        self.partial_responses_opt_out
    }

    pub fn machine_tier(&self) -> Option<&str> {
        self.identities()
            .iter()
            .find(|identity| identity.id_type() == "MACHINE_TIER")
            .map(|identity| identity.id_data())
    }
}

#[cfg(all(test, fbcode_build))]
mod tests {
    use clientinfo::ClientEntryPoint;
    use mononoke_macros::mononoke;
    use permission_checker::MononokeIdentity;

    use super::*;

    fn metadata(identities: &[(&str, &str)]) -> Metadata {
        Metadata {
            identities: identities
                .iter()
                .map(|(ty, value)| MononokeIdentity::from_legacy_type_data(*ty, *value))
                .collect(),
            ..Metadata::default()
        }
    }

    #[mononoke::test]
    fn rim_attribution_is_independent_of_main_id() {
        let mut metadata = metadata(&[
            ("SERVICE_IDENTITY", "ondemand_worker"),
            ("SERVICE_IDENTITY", "ondemand_www_fbsource_configerator"),
            ("MACHINE", "od1.ldc.facebook.com"),
        ]);
        let expected = RimTenancyPath {
            region: None,
            category: ClientCategory::DevEnv,
            allocation: None,
            workload: "www_fbsource_configerator".to_owned(),
            instance: "od1.ldc.facebook.com".to_owned(),
        };
        assert_eq!(metadata.rim_tenancy_path(None), expected);
        for main_id in ["legacy-client-a", "legacy-client-b"] {
            metadata.add_client_info(ClientInfo {
                request_info: Some(ClientRequestInfo {
                    main_id: Some(main_id.to_owned()),
                    entry_point: ClientEntryPoint::SaplingRemoteApi,
                    correlator: "test".to_owned(),
                }),
                ..ClientInfo::default()
            });
            assert_eq!(metadata.rim_tenancy_path(None), expected);
            assert_eq!(metadata.tenant_info().client_id(), Some(main_id));
            assert_eq!(
                metadata.client_request_info().unwrap().main_id.as_deref(),
                Some(main_id)
            );
        }
    }

    #[mononoke::test]
    fn rim_separates_machines_within_an_ondemand_type() {
        for machine in ["od1.ldc.facebook.com", "od2.ldc.facebook.com"] {
            let metadata = metadata(&[
                ("SERVICE_IDENTITY", "ondemand_www_fbsource_configerator"),
                ("MACHINE", machine),
            ]);
            assert_eq!(
                metadata.rim_tenancy_path(None).into_vec(),
                vec!["root", "dev_env", "www_fbsource_configerator", machine]
            );
        }
        let missing_machine =
            metadata(&[("SERVICE_IDENTITY", "ondemand_www_fbsource_configerator")]);
        assert_eq!(
            missing_machine.rim_tenancy_path(None).into_vec(),
            vec!["root", "dev_env", "www_fbsource_configerator", "unknown"]
        );
    }

    #[mononoke::test]
    fn rim_workload_identity_depends_on_category() {
        for (identities, expected) in [
            (
                vec![("USER", "alice"), ("MACHINE", "dev1")],
                vec!["root", "interactive_dev", "USER:alice", "default"],
            ),
            (
                vec![
                    ("OFFLINE_JOB_LEAF_RUN_ID", "mast-job-run/123"),
                    ("DATA_PROJECT", "project"),
                ],
                vec!["root", "mast", "DATA_PROJECT:project", "default"],
            ),
            (
                vec![
                    ("SERVICE_IDENTITY", "faas"),
                    ("ASYNC_JOB_ID", "123"),
                    ("CREWMATE", "456"),
                ],
                vec!["root", "faas", "CREWMATE:456", "default"],
            ),
            (
                vec![("SERVICE_IDENTITY", "automation")],
                vec![
                    "root",
                    "automation",
                    "SERVICE_IDENTITY:automation",
                    "default",
                ],
            ),
            (vec![], vec!["root", "unknown", "unknown", "default"]),
        ] {
            assert_eq!(
                metadata(&identities).rim_tenancy_path(None).into_vec(),
                expected
            );
        }
    }

    #[mononoke::test]
    fn rim_uses_ci_purpose_without_splitting_fetch_causes() {
        let mut metadata = metadata(&[("SERVICE_IDENTITY", "sandcastle.worker.warmed")]);
        metadata.add_client_info(
            ClientInfo::from_json(r#"{"sandcastle_alias":"build","ci_purpose":"land"}"#).unwrap(),
        );
        for cause in [
            None,
            Some("sl-prefetch"),
            Some("edenfs-prefetch"),
            Some("eden-walk-prefetch"),
        ] {
            let metadata = metadata.clone().set_fetch_cause(cause.map(str::to_owned));
            assert_eq!(
                metadata.rim_tenancy_path(None).into_vec(),
                vec!["root", "ci_sandcastle", "CI_PURPOSE:land", "default"]
            );
        }
    }

    #[mononoke::test]
    fn rim_separates_prefetch_workload_allocations() {
        for (cause, workload) in [
            (Some("sl-prefetch"), "www_fbsource_configerator:sl-prefetch"),
            (
                Some("edenfs-prefetch"),
                "www_fbsource_configerator:edenfs-prefetch",
            ),
            (
                Some("eden-walk-prefetch"),
                "www_fbsource_configerator:eden-walk-prefetch",
            ),
            (None, "www_fbsource_configerator"),
            (Some("edenfs-fs"), "www_fbsource_configerator"),
            (Some("sl-checkout"), "www_fbsource_configerator"),
            (Some("edenfs-mixed"), "www_fbsource_configerator"),
            (Some("unspecified"), "www_fbsource_configerator"),
            (Some("unknown-prefetch"), "www_fbsource_configerator"),
        ] {
            let metadata = metadata(&[
                ("SERVICE_IDENTITY", "ondemand_www_fbsource_configerator"),
                ("MACHINE", "od1.ldc.facebook.com"),
            ])
            .set_fetch_cause(cause.map(str::to_owned));
            for region in [None, Some("ldc")] {
                let expected: Vec<_> = once("root")
                    .chain(region)
                    .chain(["dev_env", workload, workload, "od1.ldc.facebook.com"])
                    .collect();
                assert_eq!(
                    metadata
                        .rim_tenancy_path(region)
                        .with_workload_allocation(|category, name| {
                            assert_eq!(category, "dev_env");
                            assert_eq!(name, workload);
                            true
                        })
                        .into_vec(),
                    expected,
                    "fetch cause: {cause:?}",
                );
            }
        }
    }

    #[mononoke::test]
    fn rim_groups_categories_under_the_serving_region() {
        let metadata = metadata(&[
            ("SERVICE_IDENTITY", "ondemand_www_fbsource_configerator"),
            ("MACHINE", "od1.prn.facebook.com"),
        ])
        .set_client_hostname(Some("od1.prn.facebook.com".to_owned()));
        assert_eq!(
            metadata.rim_tenancy_path(Some("ldc")).into_vec(),
            vec![
                "root",
                "ldc",
                "dev_env",
                "www_fbsource_configerator",
                "od1.prn.facebook.com",
            ]
        );
    }

    #[mononoke::test]
    fn rim_remaining_preserves_workloads_and_instances() {
        for workload in ["registered", "new/a", "new/b"] {
            for machine in ["od1", "od2"] {
                let metadata = metadata(&[
                    ("SERVICE_IDENTITY", &format!("ondemand_{workload}")),
                    ("MACHINE", machine),
                ]);
                for region in [None, Some("ldc")] {
                    let allocation = if workload == "registered" {
                        workload
                    } else {
                        "__mononoke_remaining__"
                    };
                    let expected: Vec<_> = once("root")
                        .chain(region)
                        .chain(["dev_env", allocation, workload, machine])
                        .collect();
                    assert_eq!(
                        metadata
                            .rim_tenancy_path(region)
                            .with_workload_allocation(|category, workload| {
                                category == "dev_env" && workload == "registered"
                            })
                            .into_vec(),
                        expected,
                    );
                }
            }
        }
    }

    #[mononoke::test]
    fn rim_workload_registration_is_scoped_to_category() {
        let metadata = metadata(&[("SERVICE_IDENTITY", "ondemand_registered")]);
        assert_eq!(
            metadata
                .rim_tenancy_path(None)
                .with_workload_allocation(|category, workload| {
                    category == "automation" && workload == "registered"
                })
                .into_vec(),
            [
                "root",
                "dev_env",
                "__mononoke_remaining__",
                "registered",
                "unknown"
            ],
        );
    }
}
