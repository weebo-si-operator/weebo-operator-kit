//! KV v2 store authenticated through Vault's Kubernetes auth (the
//! operator's own ServiceAccount JWT — no static Vault token). The login
//! is cached for 80% of its lease by [`VaultLogins`], and a KV call
//! rejected because the token is no longer valid re-logs-in once in place
//! (with the ServiceAccount token read again: a projected one rotates). A
//! call the policy denies to a valid token never re-logs-in.
//!
//! [`VaultSession`] is that login on its own: a token and a client per
//! Vault namespace it addresses, for an operator's calls beyond KV. An
//! additional mount may send its own namespace, and log in on its own (in
//! that namespace, on first use).

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock};
use vaultrs::api::kv2::requests::{SetSecretMetadataRequest, SetSecretRequestOptions};
use vaultrs::client::{Client as _, VaultClient, VaultClientSettingsBuilder};
use vaultrs::error::ClientError;
use vaultrs::{auth::kubernetes, kv2, token};
use weebo_kit_api::{SecretKeyRef, VaultAdditionalMount, VaultPathGrant};
use weebo_kit_application::ports::SecretStoreError;
use weebo_kit_domain::vault_paths::{self, PathGrant};

/// The operator's projected ServiceAccount token, exchanged for a Vault
/// token. Overridable with `VAULT_KUBERNETES_JWT_PATH`.
const DEFAULT_JWT_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// A Vault connection, as an operator's instance spec describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultConfig {
    pub address: String,
    /// KV v2 mount.
    pub mount: String,
    /// Default paths are `<path_prefix>/<namespace>/<name>` (`<namespace>/<name>`
    /// when empty).
    ///
    /// An empty prefix makes every namespace's subtree a top-level folder
    /// of the mount: a namespace named like a folder that holds other
    /// values (one a grant opens, `shared/` say) owns that folder — its
    /// objects write there without any grant. Keep a prefix unless the
    /// mount is reserved to per-namespace folders and no namespace can be
    /// created with the name of another top-level folder.
    pub path_prefix: String,
    /// The paths open to objects outside their namespace's subtree.
    pub path_grants: Vec<VaultPathGrant>,
    /// Other KV v2 mounts targets may name, each with its own grants.
    pub additional_mounts: Vec<VaultAdditionalMount>,
    pub kubernetes_auth_role: String,
    pub kubernetes_auth_mount: String,
    /// Identifies the CA bundle (e.g. `namespace/name/key` of its `Secret`
    /// plus a digest of its content), `None` for the system trust store.
    /// Only compared: a change forces a new login with the new CA, so it
    /// must change when the bundle's content does.
    pub ca_source: Option<String>,
}

impl VaultConfig {
    /// The [`ca_source`](Self::ca_source) of a CA bundle read from
    /// `secret_ref`: `namespace/name/key@sha256:<digest of pem>`.
    pub fn ca_source_of(secret_ref: &SecretKeyRef, pem: &[u8]) -> String {
        format!(
            "{}/{}/{}@sha256:{:x}",
            secret_ref.namespace,
            secret_ref.name,
            secret_ref.key,
            Sha256::digest(pem)
        )
    }
}

/// Per-request budget for every Vault call, login included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Where and how a [`VaultSession`] logs in: Vault's Kubernetes auth, in
/// `namespace` (`None`: the root, or whatever the address defaults to).
#[derive(Debug, Clone, Copy)]
pub struct VaultLogin<'a> {
    pub address: &'a str,
    /// PEM bundle of the CA, `None` for the system trust store.
    pub ca_pem: Option<&'a [u8]>,
    pub namespace: Option<&'a str>,
    pub auth_mount: &'a str,
    pub auth_role: &'a str,
    /// The ServiceAccount token exchanged for a Vault token.
    pub jwt: &'a str,
}

/// One Vault login (a token), and a client per Vault namespace it
/// addresses (`X-Vault-Namespace`), all carrying that token. A call
/// rejected because the token is no longer valid re-logs-in once
/// ([`Self::with_relogin`]); a call the policy denies to a valid token
/// never does. What an operator needs to talk to Vault beyond KV.
pub struct VaultSession {
    address: String,
    ca_pem: Option<Vec<u8>>,
    login_namespace: Option<String>,
    auth_mount: String,
    auth_role: String,
    jwt: String,
    /// Read again on every re-login when set: a projected ServiceAccount
    /// token is rotated by the kubelet, the one read at login may be
    /// expired by then.
    jwt_path: Option<std::path::PathBuf>,
    /// The current token, given to the clients of namespaces first
    /// addressed after the login.
    token: RwLock<String>,
    /// Keyed by the namespace sent (`None`: the login's). Write-locked only
    /// to add a client or swap them all after a re-login, never across a
    /// network call.
    clients: RwLock<HashMap<Option<String>, Arc<VaultClient>>>,
    cache_ttl: Option<Duration>,
}

impl VaultSession {
    pub async fn login(login: &VaultLogin<'_>) -> Result<Self, SecretStoreError> {
        let mut client = build_client(login.address, login.ca_pem, login.namespace, None)?;
        let auth = kubernetes::login(&client, login.auth_mount, login.auth_role, login.jwt.trim())
            .await
            .map_err(|e| {
                SecretStoreError(format!(
                    "Vault Kubernetes auth login (namespace {:?}, mount {:?}, role {:?}): {e}",
                    login.namespace.unwrap_or(""),
                    login.auth_mount,
                    login.auth_role
                ))
            })?;
        client.set_token(&auth.client_token);
        let login_namespace = login.namespace.map(String::from);
        Ok(Self {
            address: login.address.to_string(),
            ca_pem: login.ca_pem.map(<[u8]>::to_vec),
            login_namespace: login_namespace.clone(),
            auth_mount: login.auth_mount.to_string(),
            auth_role: login.auth_role.to_string(),
            jwt: login.jwt.trim().to_string(),
            jwt_path: None,
            token: RwLock::new(auth.client_token),
            clients: RwLock::new(HashMap::from([(login_namespace, Arc::new(client))])),
            cache_ttl: cache_ttl_from_lease(auth.lease_duration, auth.renewable),
        })
    }

    /// Re-logins read the ServiceAccount token from `path` again (falling
    /// back on the one given at login when it can't be read).
    pub fn with_jwt_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.jwt_path = Some(path.into());
        self
    }

    /// 80% of the login's lease; `None`: not to be cached.
    pub fn cache_ttl(&self) -> Option<Duration> {
        self.cache_ttl
    }

    /// The client sending `namespace` (`None`: the login's namespace),
    /// built on first use with the current token.
    pub async fn client(
        &self,
        namespace: Option<&str>,
    ) -> Result<Arc<VaultClient>, SecretStoreError> {
        let key = namespace
            .map(String::from)
            .or_else(|| self.login_namespace.clone());
        if let Some(client) = self.clients.read().await.get(&key) {
            return Ok(client.clone());
        }
        let token = self.token.read().await.clone();
        let client = Arc::new(build_client(
            &self.address,
            self.ca_pem.as_deref(),
            key.as_deref(),
            Some(&token),
        )?);
        Ok(self
            .clients
            .write()
            .await
            .entry(key)
            .or_insert(client)
            .clone())
    }

    /// `op` on the client of `namespace`, run again once after a re-login
    /// when its token was stale. The inner `Result` is Vault's answer, its
    /// HTTP status intact.
    pub async fn with_relogin<T, F, Fut>(
        &self,
        namespace: Option<&str>,
        op: F,
    ) -> Result<Result<T, ClientError>, SecretStoreError>
    where
        F: Fn(Arc<VaultClient>) -> Fut,
        Fut: Future<Output = Result<T, ClientError>>,
    {
        match op(self.client(namespace).await?).await {
            Err(e) if self.token_is_stale(&e).await => {
                self.relogin().await?;
                Ok(op(self.client(namespace).await?).await)
            }
            other => Ok(other),
        }
    }

    /// Whether `err` comes from a token that is no longer valid. Vault
    /// answers 403 both to an expired or revoked token and to a valid one
    /// its policy denies: a 403 is only a stale token when `lookup-self`
    /// (granted by the default policy of the login's namespace) fails too.
    /// Asked in the login's namespace: in another one the default policy
    /// doesn't grant it, and every denial would look stale.
    async fn token_is_stale(&self, err: &ClientError) -> bool {
        match err {
            ClientError::APIError { code: 401, .. } => true,
            ClientError::APIError { code: 403, .. } => match self.client(None).await {
                Ok(client) => token::lookup_self(&*client)
                    .await
                    .is_err_and(|e| is_auth_error(&e)),
                Err(_) => false,
            },
            _ => false,
        }
    }

    /// A new token for every client. The login runs on the login's client
    /// (it doesn't use the token); the locks are only taken for the swap.
    async fn relogin(&self) -> Result<(), SecretStoreError> {
        let fresh = self
            .jwt_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok());
        let jwt = fresh.as_deref().map_or(self.jwt.as_str(), str::trim);
        let login_client = self.client(None).await?;
        let auth = kubernetes::login(&*login_client, &self.auth_mount, &self.auth_role, jwt)
            .await
            .map_err(|e| SecretStoreError(format!("Vault re-login: {e}")))?;
        let namespaces: Vec<Option<String>> = self.clients.read().await.keys().cloned().collect();
        let mut fresh_clients = HashMap::with_capacity(namespaces.len());
        for ns in namespaces {
            let client = build_client(
                &self.address,
                self.ca_pem.as_deref(),
                ns.as_deref(),
                Some(&auth.client_token),
            )?;
            fresh_clients.insert(ns, Arc::new(client));
        }
        *self.token.write().await = auth.client_token;
        *self.clients.write().await = fresh_clients;
        Ok(())
    }
}

/// A client for `address`, sending `namespace` and carrying `token`.
/// vaultrs only reads CA certs from file paths: the PEM lives in a temp
/// file just long enough for `VaultClient::new` to load it.
fn build_client(
    address: &str,
    ca_pem: Option<&[u8]>,
    namespace: Option<&str>,
    token: Option<&str>,
) -> Result<VaultClient, SecretStoreError> {
    let mut builder = VaultClientSettingsBuilder::default();
    builder.address(address);
    // vaultrs has no default timeout: a black-holed Vault would hang every
    // call sharing this (cached) session.
    builder.timeout(Some(REQUEST_TIMEOUT));
    if let Some(ns) = namespace {
        builder.set_namespace(ns.to_string());
    }
    if let Some(token) = token {
        builder.token(token);
    }
    let _ca_file = match ca_pem {
        Some(pem) => {
            let guard = TempCaFile::write(pem)?;
            builder.ca_certs(vec![guard.path_string()]);
            Some(guard)
        }
        None => None,
    };
    let settings = builder
        .build()
        .map_err(|e| SecretStoreError(format!("Vault client settings: {e}")))?;
    VaultClient::new(settings).map_err(|e| SecretStoreError(format!("Vault client: {e}")))
}

/// The session of an additional mount with a login of its own, opened on
/// first use. A failed login is replayed for [`LOGIN_FAILURE_TTL`]: only
/// the targets on that mount fail, and Vault isn't hammered meanwhile.
enum MountSession {
    Ready(Arc<VaultSession>),
    Failed {
        error: SecretStoreError,
        until: Instant,
    },
}

pub struct VaultSecretStore {
    /// The instance's own login: its mount, and the additional mounts
    /// without a login of their own.
    session: Arc<VaultSession>,
    mount: String,
    path_prefix: String,
    path_grants: Vec<VaultPathGrant>,
    additional_mounts: Vec<VaultAdditionalMount>,
    /// By index in `additional_mounts`.
    mount_sessions: Mutex<HashMap<usize, MountSession>>,
    address: String,
    ca_pem: Option<Vec<u8>>,
    jwt: String,
    jwt_path: Option<std::path::PathBuf>,
}

impl VaultSecretStore {
    /// `ca_pem`: PEM bundle from `spec.caSecretRef`, read by the caller.
    /// Invalid `path_grants` and additional mounts fail before any login
    /// (the CRD schema refuses most of them already; this is the net for an
    /// older schema). The additional mounts with a login of their own log in
    /// on first use, not here.
    pub async fn new(
        spec: &VaultConfig,
        jwt: &str,
        ca_pem: Option<&[u8]>,
    ) -> Result<Self, SecretStoreError> {
        vault_paths::validate(&path_grants(&spec.path_grants))
            .map_err(|e| SecretStoreError(format!("Vault connection: {e}")))?;
        for (i, extra) in spec.additional_mounts.iter().enumerate() {
            if extra.mount.is_empty()
                || (extra.mount == spec.mount && extra.vault_namespace.is_none())
            {
                return Err(SecretStoreError(format!(
                    "Vault connection: additionalMounts[{i}] must name a mount other than {:?}",
                    spec.mount
                )));
            }
            if extra.auth.as_ref().is_some_and(|a| a.kubernetes.is_none()) {
                return Err(SecretStoreError(format!(
                    "Vault connection: additionalMounts[{i}].auth names no login method"
                )));
            }
            vault_paths::validate(&path_grants(&extra.path_grants)).map_err(|e| {
                SecretStoreError(format!("Vault connection: additionalMounts[{i}].{e}"))
            })?;
        }
        if let Some((ns, mount)) = VaultAdditionalMount::first_duplicate(&spec.additional_mounts) {
            return Err(SecretStoreError(format!(
                "Vault connection: additionalMounts lists mount {mount:?} twice in Vault namespace {:?}",
                ns.unwrap_or("")
            )));
        }
        let session = VaultSession::login(&VaultLogin {
            address: &spec.address,
            ca_pem,
            namespace: None,
            auth_mount: &spec.kubernetes_auth_mount,
            auth_role: &spec.kubernetes_auth_role,
            jwt,
        })
        .await?;
        Ok(Self {
            session: Arc::new(session),
            mount: spec.mount.clone(),
            path_prefix: spec.path_prefix.clone(),
            path_grants: spec.path_grants.clone(),
            additional_mounts: spec.additional_mounts.clone(),
            mount_sessions: Mutex::new(HashMap::new()),
            address: spec.address.clone(),
            ca_pem: ca_pem.map(<[u8]>::to_vec),
            jwt: jwt.trim().to_string(),
            jwt_path: None,
        })
    }

    /// Re-logins read the ServiceAccount token from `path` again (falling
    /// back on the one given to [`Self::new`] when it can't be read).
    pub fn with_jwt_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        let path = path.into();
        if let Some(session) = Arc::get_mut(&mut self.session) {
            session.jwt_path = Some(path.clone());
        }
        self.jwt_path = Some(path);
        self
    }

    /// How long the factory may reuse this store; `None` = don't cache.
    pub fn cache_ttl(&self) -> Option<Duration> {
        self.session.cache_ttl()
    }

    /// The instance's own login, for calls beyond KV.
    pub fn session(&self) -> &Arc<VaultSession> {
        &self.session
    }

    /// `<pathPrefix>/<namespace>/<name>` — one secret per emitting CR, the
    /// same convention as the Kubernetes backend's naming.
    pub fn default_path(&self, namespace: &str, name: &str) -> String {
        vault_paths::default_path(&self.path_prefix, namespace, name)
    }

    /// The normalized `path` when `writer` may write it: under
    /// `<pathPrefix>/<namespace>/`, or opened by a grant
    /// ([`vault_paths::check`]).
    pub fn check_path<'p>(
        &self,
        writer: vault_paths::Writer<'_>,
        namespace: Option<&str>,
        path: &'p str,
    ) -> Result<&'p str, vault_paths::PathDenied> {
        let grants = path_grants(&self.path_grants);
        vault_paths::check(&self.path_prefix, &grants, writer, namespace, path)
    }

    /// The KV v2 custom-metadata key naming the emitting CR.
    pub const OWNER_METADATA_KEY: &'static str = "owner";

    /// `<pathPrefix>/<namespace>/`: the subtree a namespace's CRs write to
    /// without a grant.
    pub fn namespace_prefix(&self, namespace: &str) -> String {
        vault_paths::subtree(&self.path_prefix, namespace)
    }

    /// The instance's own mount.
    pub fn mount(&self) -> &str {
        &self.mount
    }

    /// The KV operations on `mount`: the instance's own, or the only one of
    /// its additional mounts with that name.
    pub fn on_mount<'a>(&'a self, mount: &'a str) -> KvMount<'a> {
        self.on_mount_in(None, mount)
    }

    /// The KV operations on additional `mount` of Vault namespace
    /// `namespace` (`None`: the only additional mount of that name, or the
    /// instance's own).
    pub fn on_mount_in<'a>(&'a self, namespace: Option<&'a str>, mount: &'a str) -> KvMount<'a> {
        KvMount {
            store: self,
            namespace,
            mount,
        }
    }

    fn own(&self) -> KvMount<'_> {
        self.on_mount(&self.mount)
    }

    /// The grants of `mount`, `None` when it isn't an additional mount (or
    /// when several namespaces list one by that name).
    pub fn additional_mount_grants(&self, mount: &str) -> Option<&[VaultPathGrant]> {
        self.additional_mount_grants_in(None, mount).ok().flatten()
    }

    /// The grants of additional `mount` in `namespace` (`None`: the only
    /// one of that name). `Err`: several by that name, name the namespace.
    pub fn additional_mount_grants_in(
        &self,
        namespace: Option<&str>,
        mount: &str,
    ) -> Result<Option<&[VaultPathGrant]>, String> {
        VaultAdditionalMount::resolve(&self.additional_mounts, namespace, mount)
            .map(|m| m.map(|m| m.path_grants.as_slice()))
    }

    /// The normalized `path` when `writer` may write it on additional
    /// `mount`: only that mount's grants open paths
    /// ([`vault_paths::check_on_mount`]).
    pub fn check_mount_path<'p>(
        &self,
        writer: vault_paths::Writer<'_>,
        mount: &str,
        path: &'p str,
    ) -> Result<&'p str, vault_paths::PathDenied> {
        let grants = self.additional_mount_grants(mount).map(path_grants);
        vault_paths::check_on_mount(mount, grants.as_deref(), writer, path)
    }

    /// [`Self::check_mount_path`] on additional `mount` of `namespace`,
    /// the refusal as a message.
    pub fn check_mount_path_in<'p>(
        &self,
        writer: vault_paths::Writer<'_>,
        namespace: Option<&str>,
        mount: &str,
        path: &'p str,
    ) -> Result<&'p str, String> {
        let grants = self
            .additional_mount_grants_in(namespace, mount)?
            .map(path_grants);
        vault_paths::check_on_mount(mount, grants.as_deref(), writer, path)
            .map_err(|e| e.to_string())
    }

    /// The session and the namespace to send for `mount` of `namespace`:
    /// the instance's own login (with the mount's namespace, if any), or
    /// the mount's own login, opened on first use.
    async fn session_for(
        &self,
        namespace: Option<&str>,
        mount: &str,
    ) -> Result<(Arc<VaultSession>, Option<String>), SecretStoreError> {
        if namespace.is_none() && mount == self.mount {
            return Ok((self.session.clone(), None));
        }
        let extra = VaultAdditionalMount::resolve(&self.additional_mounts, namespace, mount)
            .map_err(SecretStoreError)?;
        let Some(extra) = extra else {
            return match namespace {
                // Not an additional mount: the instance's login, as before.
                None => Ok((self.session.clone(), None)),
                Some(ns) => Err(SecretStoreError(format!(
                    "no additional mount {mount:?} in Vault namespace {ns:?}"
                ))),
            };
        };
        let Some(kube) = extra.auth.as_ref().and_then(|a| a.kubernetes.as_ref()) else {
            return Ok((self.session.clone(), extra.vault_namespace.clone()));
        };
        let index = self
            .additional_mounts
            .iter()
            .position(|m| std::ptr::eq(m, extra))
            .ok_or_else(|| SecretStoreError(format!("additional mount {mount:?} not found")))?;
        let mut sessions = self.mount_sessions.lock().await;
        let now = Instant::now();
        match sessions.get(&index) {
            Some(MountSession::Ready(session)) => return Ok((session.clone(), None)),
            Some(MountSession::Failed { error, until }) if *until > now => {
                return Err(error.clone());
            }
            _ => {}
        }
        let fresh = self
            .jwt_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok());
        let jwt = fresh.as_deref().map_or(self.jwt.as_str(), str::trim);
        let login = VaultLogin {
            address: &self.address,
            ca_pem: self.ca_pem.as_deref(),
            namespace: extra.vault_namespace.as_deref(),
            auth_mount: &kube.mount,
            auth_role: &kube.role,
            jwt,
        };
        match VaultSession::login(&login).await {
            Ok(session) => {
                let session = Arc::new(match &self.jwt_path {
                    Some(path) => session.with_jwt_path(path.clone()),
                    None => session,
                });
                sessions.insert(index, MountSession::Ready(session.clone()));
                Ok((session, None))
            }
            Err(error) => {
                if let Some(until) = now.checked_add(LOGIN_FAILURE_TTL) {
                    sessions.insert(
                        index,
                        MountSession::Failed {
                            error: error.clone(),
                            until,
                        },
                    );
                }
                Err(error)
            }
        }
    }

    /// [`KvMount::owner_of`] on the instance's mount.
    pub async fn owner_of(&self, path: &str) -> Result<Option<String>, SecretStoreError> {
        self.own().owner_of(path).await
    }

    /// [`KvMount::metadata`] on the instance's mount.
    pub async fn metadata(&self, path: &str) -> Result<PathMetadata, SecretStoreError> {
        self.own().metadata(path).await
    }

    /// [`KvMount::set_owner`] on the instance's mount.
    pub async fn set_owner(&self, path: &str, owner: &str) -> Result<(), SecretStoreError> {
        self.own().set_owner(path, owner).await
    }

    /// [`KvMount::write_path`] on the instance's mount.
    pub async fn write_path(
        &self,
        path: &str,
        data: &BTreeMap<String, String>,
    ) -> Result<bool, SecretStoreError> {
        self.own().write_path(path, data).await
    }

    /// [`KvMount::write_path_cas`] on the instance's mount.
    pub async fn write_path_cas(
        &self,
        path: &str,
        data: &BTreeMap<String, String>,
        version: u64,
    ) -> Result<CasWrite, SecretStoreError> {
        self.own().write_path_cas(path, data, version).await
    }

    /// [`KvMount::read_path`] on the instance's mount.
    pub async fn read_path(
        &self,
        path: &str,
    ) -> Result<Option<BTreeMap<String, String>>, SecretStoreError> {
        self.own().read_path(path).await
    }

    /// [`KvMount::delete_path`] on the instance's mount.
    pub async fn delete_path(&self, path: &str) -> Result<(), SecretStoreError> {
        self.own().delete_path(path).await
    }
}

/// The KV v2 operations of a [`VaultSecretStore`] on one mount.
#[derive(Clone, Copy)]
pub struct KvMount<'a> {
    store: &'a VaultSecretStore,
    /// The mount's Vault namespace as the target named it.
    namespace: Option<&'a str>,
    mount: &'a str,
}

impl KvMount<'_> {
    /// The mount these operations act on.
    pub fn mount(&self) -> &str {
        self.mount
    }

    /// `op` with the mount's session, re-logged-in once on a stale token.
    async fn call<T, F, Fut>(&self, op: F) -> Result<Result<T, ClientError>, SecretStoreError>
    where
        F: Fn(Arc<VaultClient>) -> Fut,
        Fut: Future<Output = Result<T, ClientError>>,
    {
        let (session, namespace) = self.store.session_for(self.namespace, self.mount).await?;
        session.with_relogin(namespace.as_deref(), op).await
    }

    /// The owner id recorded on `path` (`Ok(None)`: nothing stored, or
    /// stored before owners were recorded, or a policy that can't read the
    /// metadata).
    pub async fn owner_of(&self, path: &str) -> Result<Option<String>, SecretStoreError> {
        Ok(match self.metadata(path).await? {
            PathMetadata::Present { owner, .. } => owner,
            PathMetadata::Absent | PathMetadata::Unreadable => None,
        })
    }

    /// What `<mount>/metadata/<path>` says: the owner recorded there and
    /// the current version, the check-and-set base of the next write.
    pub async fn metadata(&self, path: &str) -> Result<PathMetadata, SecretStoreError> {
        let mount = self.mount;
        let result = self
            .call(|client| async move { kv2::read_metadata(&*client, mount, path).await })
            .await?;
        match result {
            Ok(meta) => Ok(PathMetadata::Present {
                owner: meta
                    .custom_metadata
                    .and_then(|m| m.get(VaultSecretStore::OWNER_METADATA_KEY).cloned()),
                current_version: meta.current_version,
            }),
            Err(ClientError::APIError { code: 404, .. }) => Ok(PathMetadata::Absent),
            // A policy without `read` on `<mount>/metadata/*`: ownership
            // can't be checked, only the path confinement applies.
            Err(ClientError::APIError { code: 403, .. }) => {
                tracing::warn!(%path, "no read access to the KV metadata: secret ownership is not checked");
                Ok(PathMetadata::Unreadable)
            }
            Err(e) => Err(SecretStoreError(format!("Vault read metadata {path}: {e}"))),
        }
    }

    /// Records `owner` on `path`'s metadata. Best effort: a policy without
    /// `create`/`update` on `<mount>/metadata/*` only loses the ownership
    /// record (warned), not the write.
    pub async fn set_owner(&self, path: &str, owner: &str) -> Result<(), SecretStoreError> {
        let mount = self.mount;
        let result = self
            .call(|client| async move {
                let mut opts = SetSecretMetadataRequest::builder();
                opts.custom_metadata(HashMap::from([(
                    VaultSecretStore::OWNER_METADATA_KEY.to_string(),
                    owner.to_string(),
                )]));
                kv2::set_metadata(&*client, mount, path, Some(&mut opts)).await
            })
            .await?;
        match result {
            Ok(()) => Ok(()),
            Err(ClientError::APIError { code: 403, .. }) => {
                tracing::warn!(%path, "no write access to the KV metadata: secret ownership is not recorded");
                Ok(())
            }
            Err(e) => Err(SecretStoreError(format!(
                "Vault write metadata {path}: {e}"
            ))),
        }
    }

    /// KV v2 `set` creates a new version even for identical data, so the
    /// current document is compared first: steady-state reconciles never
    /// churn the version history. `true` when a new version was written.
    pub async fn write_path(
        &self,
        path: &str,
        data: &BTreeMap<String, String>,
    ) -> Result<bool, SecretStoreError> {
        if self.read_path(path).await.ok().flatten().as_ref() == Some(data) {
            return Ok(false);
        }
        let mount = self.mount;
        self.call(|client| async move { kv2::set(&*client, mount, path, data).await })
            .await?
            .map(|_| true)
            .map_err(|e| SecretStoreError(format!("Vault write {path}: {e}")))
    }

    /// [`Self::write_path`] with KV v2 check-and-set: the write only lands
    /// if `path`'s current version is still `version` (`0`: nothing was
    /// ever written there — two creators racing, only one wins). Identical
    /// data is not rewritten ([`CasWrite::Unchanged`]); a version that
    /// moved on is [`CasWrite::Conflict`], not an error.
    pub async fn write_path_cas(
        &self,
        path: &str,
        data: &BTreeMap<String, String>,
        version: u64,
    ) -> Result<CasWrite, SecretStoreError> {
        if self.read_path(path).await.ok().flatten().as_ref() == Some(data) {
            return Ok(CasWrite::Unchanged);
        }
        let cas = u32::try_from(version).map_err(|_| {
            SecretStoreError(format!(
                "Vault write {path}: version {version} out of range"
            ))
        })?;
        let mount = self.mount;
        let result = self
            .call(|client| async move {
                let options = SetSecretRequestOptions { cas };
                kv2::set_with_options(&*client, mount, path, data, options).await
            })
            .await?;
        match result {
            Ok(_) => Ok(CasWrite::Written),
            Err(e) if is_cas_mismatch(&e) => Ok(CasWrite::Conflict),
            Err(e) => Err(SecretStoreError(format!("Vault write {path}: {e}"))),
        }
    }

    /// `Ok(None)` when nothing is stored at `path`.
    pub async fn read_path(
        &self,
        path: &str,
    ) -> Result<Option<BTreeMap<String, String>>, SecretStoreError> {
        let mount = self.mount;
        let result = self
            .call(|client| async move {
                kv2::read::<BTreeMap<String, String>>(&*client, mount, path).await
            })
            .await?;
        match result {
            Ok(data) => Ok(Some(data)),
            Err(ClientError::APIError { code: 404, .. }) => Ok(None),
            Err(e) => Err(SecretStoreError(format!("Vault read {path}: {e}"))),
        }
    }

    /// Deletes every version and the metadata — not a recoverable soft
    /// delete — given `delete` on `<mount>/metadata/*`. Without it, best
    /// effort: the current version is soft-deleted (`delete` on
    /// `<mount>/data/*`), the older ones stay in the history (warned).
    /// Idempotent.
    pub async fn delete_path(&self, path: &str) -> Result<(), SecretStoreError> {
        let mount = self.mount;
        let result = self
            .call(|client| async move { kv2::delete_metadata(&*client, mount, path).await })
            .await?;
        let result = match result {
            Err(ClientError::APIError { code: 403, .. }) => {
                tracing::warn!(%path, "no delete access to the KV metadata: only the current version is deleted, older ones stay");
                self.call(|client| async move { kv2::delete_latest(&*client, mount, path).await })
                    .await?
            }
            other => other,
        };
        match result {
            Ok(()) | Err(ClientError::APIError { code: 404, .. }) => Ok(()),
            Err(e) => Err(SecretStoreError(format!("Vault delete {path}: {e}"))),
        }
    }
}

/// What the KV metadata of a path says ([`VaultSecretStore::metadata`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathMetadata {
    /// Nothing at the path: no value, no metadata.
    Absent,
    /// `owner`: the id recorded, `None` for a value written before owners
    /// were recorded (or by hand). `current_version`: `0` when only the
    /// metadata exists (an owner recorded before the first write); a
    /// deleted latest version still counts.
    Present {
        owner: Option<String>,
        current_version: u64,
    },
    /// The policy can't read `<mount>/metadata/*`: ownership isn't checked.
    Unreadable,
}

/// The outcome of [`VaultSecretStore::write_path_cas`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasWrite {
    /// A new version was written.
    Written,
    /// The path already held this data: nothing written.
    Unchanged,
    /// The path's version moved since it was read: someone else wrote.
    Conflict,
}

/// KV v2's refusal of a check-and-set write whose version doesn't match.
fn is_cas_mismatch(err: &ClientError) -> bool {
    matches!(err, ClientError::APIError { code: 400, errors }
        if errors.iter().any(|e| e.contains("check-and-set")))
}

enum Cached {
    Login {
        config: VaultConfig,
        store: Arc<VaultSecretStore>,
        expires: Instant,
    },
    /// A failed login, replayed until `expires` instead of hammering an
    /// unreachable or misconfigured Vault on every reconcile.
    Failed {
        config: VaultConfig,
        error: VaultLoginError,
        expires: Instant,
    },
}

impl Cached {
    fn expires(&self) -> Instant {
        match self {
            Self::Login { expires, .. } | Self::Failed { expires, .. } => *expires,
        }
    }
}

type Slot = Arc<Mutex<Option<Cached>>>;

/// Why [`VaultLogins::get`] has no store. `Clone`: a failed login is
/// replayed for [`LOGIN_FAILURE_TTL`].
#[derive(Debug, Clone, thiserror::Error)]
pub enum VaultLoginError {
    #[error("reading the ServiceAccount token {path}: {source}")]
    Token {
        path: String,
        source: Arc<std::io::Error>,
    },
    /// The configured CA could not be loaded (`load_ca`'s error).
    #[error("{0}")]
    Ca(String),
    #[error("{}", .0.0)]
    Login(SecretStoreError),
}

/// How long a failed login is replayed before the next attempt.
pub const LOGIN_FAILURE_TTL: Duration = Duration::from_secs(15);

/// Vault logins cached per instance for 80% of their lease (each login
/// mints a Vault token that lives its whole TTL — logging in on every
/// reconcile churned tokens), and redone when the instance's Vault
/// settings change. Logins of different instances never wait for each
/// other: each instance has its own lock, held across its login only.
/// A slot whose login has expired is dropped on the next call, so a deleted
/// instance's token is not kept past its cache TTL.
#[derive(Default)]
pub struct VaultLogins {
    slots: std::sync::Mutex<HashMap<String, Slot>>,
}

impl VaultLogins {
    /// The store of instance `key`, logging in when none is cached for
    /// `config`. `load_ca` (the PEM of the configured CA, if any) only runs
    /// on a login.
    pub async fn get<F, Fut>(
        &self,
        key: &str,
        config: &VaultConfig,
        load_ca: F,
    ) -> Result<Arc<VaultSecretStore>, VaultLoginError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<Vec<u8>>, String>>,
    {
        let slot = {
            let mut slots = self.slots.lock().unwrap_or_else(|p| p.into_inner());
            // Only on a new key: the map can't grow without it.
            if !slots.contains_key(key) {
                prune(&mut slots, key, Instant::now());
            }
            slots.entry(key.to_string()).or_default().clone()
        };
        let mut cached = slot.lock().await;
        let now = Instant::now();
        match &*cached {
            Some(Cached::Login {
                config: c,
                store,
                expires,
            }) if c == config && *expires > now => return Ok(store.clone()),
            Some(Cached::Failed {
                config: c,
                error,
                expires,
            }) if c == config && *expires > now => return Err(error.clone()),
            _ => {}
        }

        match Self::login(config, load_ca).await {
            Ok(store) => {
                // A lease too long for the clock (`checked_add`) isn't cached.
                *cached = store
                    .cache_ttl()
                    .and_then(|ttl| Instant::now().checked_add(ttl))
                    .map(|expires| Cached::Login {
                        config: config.clone(),
                        store: store.clone(),
                        expires,
                    });
                Ok(store)
            }
            Err(error) => {
                *cached = Instant::now()
                    .checked_add(LOGIN_FAILURE_TTL)
                    .map(|expires| Cached::Failed {
                        config: config.clone(),
                        error: error.clone(),
                        expires,
                    });
                Err(error)
            }
        }
    }

    async fn login<F, Fut>(
        config: &VaultConfig,
        load_ca: F,
    ) -> Result<Arc<VaultSecretStore>, VaultLoginError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Option<Vec<u8>>, String>>,
    {
        let jwt_path =
            std::env::var("VAULT_KUBERNETES_JWT_PATH").unwrap_or_else(|_| DEFAULT_JWT_PATH.into());
        let jwt = std::fs::read_to_string(&jwt_path).map_err(|e| VaultLoginError::Token {
            path: jwt_path.clone(),
            source: Arc::new(e),
        })?;
        let ca = load_ca().await.map_err(VaultLoginError::Ca)?;
        VaultSecretStore::new(config, &jwt, ca.as_deref())
            .await
            .map(|store| Arc::new(store.with_jwt_path(jwt_path)))
            .map_err(VaultLoginError::Login)
    }
}

/// Drops the slots, other than `keep`'s, that nobody is using and that hold
/// no live login: what is left is bounded by the instances seen within one
/// cache TTL.
fn prune(slots: &mut HashMap<String, Slot>, keep: &str, now: Instant) {
    slots.retain(|key, slot| {
        key == keep
            || Arc::strong_count(slot) > 1
            || slot
                .try_lock()
                .map_or(true, |c| c.as_ref().is_some_and(|c| c.expires() > now))
    });
}

/// 80% of the login lease; a zero lease or non-renewable token isn't
/// cached at all. The lease comes from Vault: saturating, not overflowing.
fn cache_ttl_from_lease(lease_duration: u64, renewable: bool) -> Option<Duration> {
    (lease_duration > 0 && renewable)
        .then(|| Duration::from_secs(lease_duration.saturating_mul(4) / 5))
}

/// 401/403: the token is no good (expired or revoked), or — 403 — the
/// policy denies the call ([`VaultSecretStore::token_is_stale`] tells).
fn is_auth_error(err: &ClientError) -> bool {
    matches!(err, ClientError::APIError { code, .. } if *code == 401 || *code == 403)
}

/// The domain's view of the instance's grants.
fn path_grants(grants: &[VaultPathGrant]) -> Vec<PathGrant<'_>> {
    grants
        .iter()
        .map(|g| PathGrant {
            namespaces: &g.namespaces,
            cluster_scoped: g.cluster_scoped,
            paths: &g.paths,
        })
        .collect()
}

/// A CA PEM in a temp file for one `VaultClient::new`, removed on drop.
struct TempCaFile {
    path: std::path::PathBuf,
}

impl TempCaFile {
    fn write(pem: &[u8]) -> Result<Self, SecretStoreError> {
        let path =
            std::env::temp_dir().join(format!("weebo-vault-ca-{}.pem", uuid::Uuid::new_v4()));
        std::fs::write(&path, pem).map_err(|e| {
            SecretStoreError(format!("writing Vault CA to {}: {e}", path.display()))
        })?;
        Ok(Self { path })
    }

    fn path_string(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

impl Drop for TempCaFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            // Only a leftover file in the temp dir: the CA is public.
            tracing::warn!(path = %self.path.display(), error = %e, "removing the Vault CA temp file");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_caching_rules() {
        assert_eq!(
            cache_ttl_from_lease(100, true),
            Some(Duration::from_secs(80))
        );
        assert_eq!(cache_ttl_from_lease(0, true), None);
        assert_eq!(cache_ttl_from_lease(100, false), None);
        assert_eq!(
            cache_ttl_from_lease(u64::MAX, true),
            Some(Duration::from_secs(u64::MAX / 5))
        );
    }

    #[test]
    fn prune_drops_idle_empty_slots_only() {
        let mut slots: HashMap<String, Slot> = HashMap::new();
        for key in ["gone", "kept", "busy"] {
            slots.insert(key.into(), Slot::default());
        }
        let busy = slots["busy"].clone();
        prune(&mut slots, "kept", Instant::now());
        let mut left: Vec<_> = slots.keys().map(String::as_str).collect();
        left.sort_unstable();
        assert_eq!(left, ["busy", "kept"]);
        drop(busy);
    }

    #[test]
    fn ca_source_changes_with_the_bundle() {
        let r = SecretKeyRef {
            name: "ca".into(),
            namespace: "vault".into(),
            key: "ca.crt".into(),
        };
        let a = VaultConfig::ca_source_of(&r, b"one");
        assert!(a.starts_with("vault/ca/ca.crt@sha256:"), "{a}");
        assert_ne!(a, VaultConfig::ca_source_of(&r, b"two"));
    }

    #[test]
    fn temp_ca_file_is_removed_on_drop() {
        let path = {
            let guard = TempCaFile::write(b"pem").unwrap();
            let path = std::path::PathBuf::from(guard.path_string());
            assert_eq!(std::fs::read(&path).unwrap(), b"pem");
            path
        };
        assert!(!path.exists());
    }
}
