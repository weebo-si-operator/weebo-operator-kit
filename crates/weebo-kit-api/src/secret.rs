//! Where emitted secrets go.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecretStoreBackend {
    #[default]
    Kubernetes,
    Vault,
}

/// Where an emitted secret (generated password or key, OAuth2 credentials,
/// access token) is written. A CR may list several targets — the same
/// value is fanned out to each.
#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-weebo-produces" = {
    "kind": { "kind": "Secret" },
    "name": ["name"],
    "namespace": "namespace",
    "unless": { "path": "backend", "equals": "vault" }
}))]
pub struct SecretTarget {
    /// `kubernetes` (default) or `vault`. A Vault target reuses the Vault
    /// connection (address, mount, auth) the instance configures, which it
    /// must then do.
    #[serde(default)]
    pub backend: SecretStoreBackend,
    /// Kubernetes: the `Secret` name. Vault: names the default path
    /// `<pathPrefix>/<namespace>/<name>` when `path` is unset.
    pub name: String,
    /// Kubernetes `Secret` namespace (Vault: the namespace segment of the
    /// default path). Defaults to the CR's own namespace; required on
    /// cluster-scoped CRs. A namespaced CR may only name its own namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Vault only: explicit KV v2 path under the instance's mount. It must
    /// stay under `<pathPrefix>/<namespace>/` (this object's namespace, or
    /// the target's on a cluster-scoped object), unless the instance's
    /// `pathGrants` open it to this object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Vault only: a KV v2 mount other than the instance's own, one the
    /// instance lists in `additionalMounts`. Needs `path`, which only that
    /// mount's `pathGrants` can open: an additional mount has no
    /// per-namespace subtree. Unset (or the instance's mount): the
    /// instance's mount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount: Option<String>,
    /// Vault only, with `mount`: the Vault namespace of that additional
    /// mount, when the instance lists two additional mounts of the same name
    /// in different Vault namespaces. Unset: the only additional mount of
    /// that name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128), regex(pattern = VAULT_NAMESPACE_PATTERN))]
    pub vault_namespace: Option<String>,
}

/// A Vault namespace path: segments of letters, digits, `_` and `-`
/// separated by `/`, no leading or trailing `/`.
pub const VAULT_NAMESPACE_PATTERN: &str = r"^[A-Za-z0-9_-]+(/[A-Za-z0-9_-]+)*$";

/// A path pattern: segments separated by `/`, each a literal, `+` (any one
/// segment) or, last only, `*` (one or more segments).
pub const PATH_PATTERN: &str = r"^(?:(?:[^/*+]+|\+)/)*(?:[^/*+]+|\+|\*)$";

/// Bounds keeping the CEL rules' estimated cost within the apiserver's
/// budget. The field embedding a list of grants must bound it too
/// (`#[schemars(length(max = 32))]`): unbounded, the apiserver refuses the
/// CRD.
const MAX_PATHS: u32 = 64;
const MAX_PATH_LENGTH: u32 = 512;
const MAX_NAMESPACES: u32 = 64;

/// A KV v2 mount besides the instance's own that objects may write to,
/// only at the paths its grants open: nothing in it is any namespace's by
/// default. The Vault policy of the operator's role stays the hard bound.
#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VaultAdditionalMount {
    /// The KV v2 mount, as targets name it in their `mount`.
    #[schemars(length(min = 1, max = 128))]
    pub mount: String,
    /// The paths of this mount objects may write to.
    // 8, not 32 like `pathGrants`: nested under a list of mounts, 32 puts
    // the paths' CEL rules over the apiserver's cost budget.
    #[schemars(length(min = 1, max = 8))]
    pub path_grants: Vec<VaultPathGrant>,
    /// The Vault namespace the mount lives in (sent as `X-Vault-Namespace`).
    /// Unset: the namespace of the instance's own login. Two additional
    /// mounts may share a name only in different namespaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128), regex(pattern = VAULT_NAMESPACE_PATTERN))]
    pub vault_namespace: Option<String>,
    /// A login of its own for this mount, done in `vaultNamespace` on first
    /// use. Unset: the instance's login. A failed login only fails the
    /// targets on this mount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<VaultMountAuth>,
}

impl VaultAdditionalMount {
    /// The additional mount a target names: `mount` in `namespace`, or,
    /// with no namespace, the only additional mount called `mount`.
    /// `Ok(None)`: no additional mount by that name. `Err`: several, the
    /// target has to name its namespace.
    pub fn resolve<'m>(
        mounts: &'m [Self],
        namespace: Option<&str>,
        mount: &str,
    ) -> Result<Option<&'m Self>, String> {
        let mut named = mounts.iter().filter(|m| m.mount == mount);
        if let Some(ns) = namespace {
            return Ok(named.find(|m| m.vault_namespace.as_deref() == Some(ns)));
        }
        match (named.next(), named.next()) {
            (Some(_), Some(_)) => Err(format!(
                "mount {mount:?} is listed in several Vault namespaces: name its vaultNamespace"
            )),
            (first, _) => Ok(first),
        }
    }

    /// The first `(vaultNamespace, mount)` pair listed twice, if any.
    pub fn first_duplicate(mounts: &[Self]) -> Option<(Option<&str>, &str)> {
        mounts.iter().enumerate().find_map(|(i, m)| {
            let key = (m.vault_namespace.as_deref(), m.mount.as_str());
            mounts
                .get(..i)
                .is_some_and(|before| {
                    before
                        .iter()
                        .any(|b| (b.vault_namespace.as_deref(), b.mount.as_str()) == key)
                })
                .then_some(key)
        })
    }
}

/// How an additional mount logs in. One field per method; `kubernetes`
/// only for now.
#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VaultMountAuth {
    /// Vault's Kubernetes auth, with the operator's ServiceAccount token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<VaultKubernetesAuth>,
}

/// A Kubernetes auth login: the auth mount and the role, in the mount's
/// Vault namespace.
#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VaultKubernetesAuth {
    /// The Kubernetes auth mount, e.g. `kubernetes`.
    #[schemars(length(min = 1, max = 128))]
    pub mount: String,
    /// The role to log in with.
    #[schemars(length(min = 1, max = 128))]
    pub role: String,
}

/// Paths of the instance's secret backend that objects may write to
/// outside their namespace's subtree `<pathPrefix>/<namespace>/`. Opens
/// them to every subject allowed to create an emitting object in a
/// covered namespace, and cannot hand over a value another object owns.
#[derive(Serialize, Deserialize, Clone, Debug, Default, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [{
    "rule": "(has(self.namespaces) && size(self.namespaces) > 0) != (has(self.clusterScoped) && self.clusterScoped)",
    "message": "a grant names namespaces or is clusterScoped, not both"
}]))]
pub struct VaultPathGrant {
    /// Namespaces whose objects may write `paths`: exact names, `*`, or a
    /// prefix glob such as `team-*`. `team-*` opens the paths to every
    /// matching namespace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(schema_with = "namespace_patterns")]
    pub namespaces: Vec<String>,
    /// The grant opens `paths` to cluster-scoped objects (instead of
    /// `namespaces`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cluster_scoped: bool,
    /// Path patterns under the mount, compared segment by segment: a
    /// literal segment matches itself, `+` exactly one segment, and a last
    /// `*` one or more. Each holds at least one literal segment, e.g.
    /// `shared/auth`, `+/auth`, `platform/*`. Prefer exact paths where
    /// values not written by the operator live: a covered object may take
    /// over a value no object owns yet.
    #[schemars(schema_with = "path_patterns")]
    pub paths: Vec<String>,
}

fn namespace_patterns(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "maxItems": MAX_NAMESPACES,
        "items": {"type": "string", "minLength": 1, "maxLength": 64},
    })
}

fn path_patterns(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type": "array",
        "minItems": 1,
        "maxItems": MAX_PATHS,
        "items": {
            "type": "string",
            "maxLength": MAX_PATH_LENGTH,
            "pattern": PATH_PATTERN,
            "x-kubernetes-validations": [
                {
                    "rule": r"!self.matches('(^|/)\\.\\.?(/|$)')",
                    "message": "a path pattern has no \".\" or \"..\" segment",
                },
                {
                    "rule": "self.matches('(^|/)[^/*+]+(/|$)')",
                    "message": "a path pattern has at least one literal segment",
                },
            ],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_defaults_to_kubernetes() {
        let t: SecretTarget = serde_json::from_value(serde_json::json!({"name": "creds"})).unwrap();
        assert_eq!(t.backend, SecretStoreBackend::Kubernetes);
        let v: SecretTarget =
            serde_json::from_value(serde_json::json!({"name": "c", "backend": "vault"})).unwrap();
        assert_eq!(v.backend, SecretStoreBackend::Vault);
    }

    fn extra(mount: &str, ns: Option<&str>) -> VaultAdditionalMount {
        VaultAdditionalMount {
            mount: mount.into(),
            vault_namespace: ns.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn a_mount_is_resolved_by_name_then_by_namespace() {
        let mounts = [
            extra("users", Some("users")),
            extra("orgs", Some("devenv")),
            extra("orgs", Some("prod")),
        ];
        let found = |ns, m| {
            VaultAdditionalMount::resolve(&mounts, ns, m)
                .map(|o| o.and_then(|m| m.vault_namespace.clone()))
        };
        assert_eq!(found(None, "users"), Ok(Some("users".into())));
        assert_eq!(found(Some("prod"), "orgs"), Ok(Some("prod".into())));
        assert_eq!(found(Some("recette"), "orgs"), Ok(None));
        assert_eq!(found(None, "mv"), Ok(None));
        assert!(found(None, "orgs").unwrap_err().contains("vaultNamespace"));
    }

    #[test]
    fn a_pair_listed_twice_is_found() {
        let ok = [extra("orgs", Some("devenv")), extra("orgs", Some("prod"))];
        assert_eq!(VaultAdditionalMount::first_duplicate(&ok), None);
        let twice = [
            extra("orgs", Some("prod")),
            extra("users", None),
            extra("orgs", Some("prod")),
        ];
        assert_eq!(
            VaultAdditionalMount::first_duplicate(&twice),
            Some((Some("prod"), "orgs"))
        );
    }

    #[test]
    fn the_namespace_fields_carry_their_pattern() {
        let schema = serde_json::to_value(schemars::schema_for!(VaultAdditionalMount)).unwrap();
        assert_eq!(
            schema["properties"]["vaultNamespace"]["pattern"],
            VAULT_NAMESPACE_PATTERN
        );
        let target = serde_json::to_value(schemars::schema_for!(SecretTarget)).unwrap();
        assert_eq!(target["properties"]["vaultNamespace"]["maxLength"], 128);
    }

    #[test]
    fn the_grant_schema_carries_its_bounds_and_rules() {
        let schema = serde_json::to_value(schemars::schema_for!(VaultPathGrant)).unwrap();
        let paths = &schema["properties"]["paths"];
        assert_eq!(paths["minItems"], 1);
        assert_eq!(paths["items"]["pattern"], PATH_PATTERN);
        assert_eq!(
            paths["items"]["x-kubernetes-validations"][1]["rule"],
            "self.matches('(^|/)[^/*+]+(/|$)')"
        );
        assert!(
            paths["description"]
                .as_str()
                .unwrap()
                .contains("segment by segment")
        );
        assert_eq!(schema["required"], serde_json::json!(["paths"]));
        assert_eq!(
            schema["x-kubernetes-validations"][0]["message"],
            "a grant names namespaces or is clusterScoped, not both"
        );
    }
}
