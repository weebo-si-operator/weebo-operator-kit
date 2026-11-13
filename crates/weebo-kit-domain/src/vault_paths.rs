//! Which KV paths an emitting object may write to: its namespace's subtree
//! `<pathPrefix>/<namespace>/`, or a path the instance's `pathGrants`
//! open to it. Shared by the secret stores and the admission webhooks, so
//! both refuse the same paths with the same message.
//!
//! Paths and grant patterns are compared segment by segment, never as
//! string prefixes. A pattern's segment is a literal, `+` (exactly one
//! segment), a `{name}` capture or, last only, `*` (one or more segments),
//! and every pattern holds at least one literal or capture.
//!
//! A namespace pattern may capture part of the writer's namespace once,
//! `dev-ws-{user}`: a `{user}` path segment then matches only that value,
//! so `dev-ws-max` may write `max/token` and not `other/token`.

use std::fmt;

use crate::allow_list::namespace_matches;

/// One `pathGrants` entry, borrowed from the instance's spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathGrant<'a> {
    /// Namespace patterns (`name`, `*`, `team-*`, `dev-ws-{user}`) the
    /// grant opens `paths` to. Empty on a cluster-scoped grant.
    pub namespaces: &'a [String],
    /// The grant opens `paths` to cluster-scoped objects, and only to them.
    pub cluster_scoped: bool,
    pub paths: &'a [String],
}

/// Who writes: the emitting object, by scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer<'a> {
    Namespaced(&'a str),
    Cluster,
}

/// Why a path is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathDenied {
    Malformed {
        path: String,
        why: &'static str,
    },
    /// Out of the writer's subtree (`None`: a cluster-scoped writer naming
    /// no namespace has none), and no grant covers it.
    NotGranted {
        path: String,
        subtree: Option<String>,
        namespace: Option<String>,
    },
    /// A mount the instance doesn't list among its additional mounts.
    UnknownMount {
        mount: String,
    },
    /// On an additional mount, which has no subtree: no grant of that
    /// mount covers the path for this writer.
    NotGrantedOnMount {
        path: String,
        mount: String,
        namespace: Option<String>,
    },
}

impl fmt::Display for PathDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathDenied::Malformed { path, why } => write!(f, "Vault path {path:?} {why}"),
            PathDenied::NotGranted {
                path,
                subtree,
                namespace,
            } => {
                match subtree {
                    Some(subtree) => write!(f, "Vault path {path:?} is outside {subtree:?}")?,
                    None => write!(f, "Vault path {path:?} names no namespace")?,
                }
                match namespace {
                    Some(ns) => write!(
                        f,
                        " and no pathGrants entry of the instance allows it for namespace {ns:?}"
                    ),
                    None => write!(
                        f,
                        " and no pathGrants entry of the instance allows it for cluster-scoped objects"
                    ),
                }
            }
            PathDenied::UnknownMount { mount } => write!(
                f,
                "Vault mount {mount:?} is not one of the instance's additionalMounts"
            ),
            PathDenied::NotGrantedOnMount {
                path,
                mount,
                namespace,
            } => match namespace {
                Some(ns) => write!(
                    f,
                    "Vault path {path:?} on mount {mount:?}: no pathGrants entry of that mount allows it for namespace {ns:?}"
                ),
                None => write!(
                    f,
                    "Vault path {path:?} on mount {mount:?}: no pathGrants entry of that mount allows it for cluster-scoped objects"
                ),
            },
        }
    }
}

impl std::error::Error for PathDenied {}

/// Why a grant is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantError(pub String);

impl fmt::Display for GrantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GrantError {}

/// `path` without its leading `/`s. Refused when empty, or holding an
/// empty, `.` or `..` segment.
pub fn normalize(path: &str) -> Result<&str, PathDenied> {
    let normalized = path.trim_start_matches('/');
    let why = if normalized.is_empty() {
        Some("is empty")
    } else if normalized.split('/').any(str::is_empty) {
        Some("has an empty segment")
    } else if normalized.split('/').any(|s| s == "." || s == "..") {
        Some("has a \".\" or \"..\" segment")
    } else {
        None
    };
    match why {
        Some(why) => Err(PathDenied::Malformed {
            path: path.to_string(),
            why,
        }),
        None => Ok(normalized),
    }
}

/// `<prefix>/<namespace>/`, or `<namespace>/` with an empty prefix: the
/// subtree of a namespace's objects.
///
/// With an empty prefix every namespace name is a top-level folder of the
/// mount: a namespace named like a folder holding other values (one a
/// grant opens, say `shared/`) owns that folder, grant or not. An instance
/// should only use it on a mount reserved to per-namespace folders.
pub fn subtree(prefix: &str, namespace: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        format!("{namespace}/")
    } else {
        format!("{prefix}/{namespace}/")
    }
}

/// `<prefix>/<namespace>/<name>`, the path of a target without one.
pub fn default_path(prefix: &str, namespace: &str, name: &str) -> String {
    format!("{}{name}", subtree(prefix, namespace))
}

/// Whether a grant covers `writer` writing the normalized `path`. An
/// invalid pattern covers nothing.
pub fn granted(grants: &[PathGrant<'_>], writer: Writer<'_>, path: &str) -> bool {
    grants.iter().any(|grant| {
        let covers = |capture: Option<Capture<'_>>| {
            grant.paths.iter().any(|pattern| {
                check_pattern(pattern).is_ok() && pattern_matches(pattern, path, capture)
            })
        };
        match writer {
            Writer::Namespaced(ns) => {
                !grant.cluster_scoped
                    && grant
                        .namespaces
                        .iter()
                        .filter_map(|p| namespace_capture(p, ns))
                        .any(covers)
            }
            Writer::Cluster => grant.cluster_scoped && covers(None),
        }
    })
}

/// A `{name}` capture and the value it took from the writer's namespace.
type Capture<'a> = (&'a str, &'a str);

/// The name of a `{name}` segment: a lowercase letter, then lowercase
/// letters, digits or `_`.
fn capture_name(segment: &str) -> Option<&str> {
    let name = segment.strip_prefix('{')?.strip_suffix('}')?;
    valid_name(name).then_some(name)
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A namespace pattern with a capture, split as (prefix, name, suffix).
/// `None`: no capture, or a malformed one (see [`check_namespace_pattern`]).
fn split_capture(pattern: &str) -> Option<(&str, &str, &str)> {
    let (prefix, rest) = pattern.split_once('{')?;
    let (name, suffix) = rest.split_once('}')?;
    let plain = |s: &str| !s.contains(['{', '}', '*']);
    (valid_name(name) && plain(prefix) && plain(suffix)).then_some((prefix, name, suffix))
}

/// Whether namespace pattern `pattern` matches `ns`: `Some(capture)` when
/// it does (`None` inside: it captures nothing). A malformed pattern
/// matches nothing.
fn namespace_capture<'a>(pattern: &'a str, ns: &'a str) -> Option<Option<Capture<'a>>> {
    if !pattern.contains(['{', '}']) {
        return namespace_matches(pattern, ns).then_some(None);
    }
    let (prefix, name, suffix) = split_capture(pattern)?;
    let value = ns.strip_prefix(prefix)?.strip_suffix(suffix)?;
    // A namespace name holds no `/`; checked anyway, the value is a segment.
    let segment = !value.is_empty() && !value.contains('/') && value != "." && value != "..";
    segment.then_some(Some((name, value)))
}

fn check_namespace_pattern(pattern: &str) -> Result<Option<&str>, String> {
    if !pattern.contains(['{', '}']) {
        return Ok(None);
    }
    split_capture(pattern)
        .map(|(_, name, _)| Some(name))
        .ok_or_else(|| {
            format!(
                "namespace pattern {pattern:?} has a malformed capture: one {{name}} (lowercase \
             letters, digits, \"_\"), no \"*\""
            )
        })
}

fn pattern_matches(pattern: &str, path: &str, capture: Option<Capture<'_>>) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    let (fixed, rest) = match pattern.split_last() {
        Some((&"*", fixed)) => (fixed, true),
        _ => (pattern.as_slice(), false),
    };
    let lengths = if rest {
        path.len() > fixed.len()
    } else {
        path.len() == fixed.len()
    };
    lengths
        && fixed.iter().zip(&path).all(|(p, s)| match capture_name(p) {
            Some(name) => capture == Some((name, s)),
            None => *p == "+" || p == s,
        })
}

fn check_pattern(pattern: &str) -> Result<(), String> {
    let segments: Vec<&str> = pattern.split('/').collect();
    let last = segments.len().saturating_sub(1);
    for (i, segment) in segments.iter().enumerate() {
        let why = match *segment {
            "" => Some("has an empty segment (or a leading \"/\")"),
            "." | ".." => Some("has a \".\" or \"..\" segment"),
            "*" if i != last => Some("has \"*\" before its last segment"),
            "*" | "+" => None,
            s if s.contains(['*', '+']) => Some("mixes \"*\" or \"+\" with other characters"),
            s if s.contains(['{', '}']) && capture_name(s).is_none() => {
                Some("has a brace outside a whole {name} segment")
            }
            _ => None,
        };
        if let Some(why) = why {
            return Err(format!("path pattern {pattern:?} {why}"));
        }
    }
    if segments.iter().all(|s| *s == "*" || *s == "+") {
        return Err(format!(
            "path pattern {pattern:?} has no literal segment: it would cover the whole mount"
        ));
    }
    Ok(())
}

/// Every grant names namespaces or is cluster-scoped (not both), and
/// lists at least one valid path pattern. A `{name}` path segment needs
/// every namespace pattern of its grant to capture `name`.
pub fn validate(grants: &[PathGrant<'_>]) -> Result<(), GrantError> {
    for (i, grant) in grants.iter().enumerate() {
        let error = |why: String| GrantError(format!("pathGrants[{i}]: {why}"));
        match (grant.namespaces.is_empty(), grant.cluster_scoped) {
            (true, false) => {
                return Err(error("names no namespace and is not clusterScoped".into()));
            }
            (false, true) => return Err(error("names namespaces and is clusterScoped".into())),
            _ => {}
        }
        if grant.namespaces.iter().any(String::is_empty) {
            return Err(error("has an empty namespace pattern".into()));
        }
        if grant.paths.is_empty() {
            return Err(error("lists no path".into()));
        }
        let captures = grant
            .namespaces
            .iter()
            .map(|p| check_namespace_pattern(p))
            .collect::<Result<Vec<_>, _>>()
            .map_err(error)?;
        for pattern in grant.paths {
            check_pattern(pattern).map_err(error)?;
            for name in pattern.split('/').filter_map(capture_name) {
                if grant.cluster_scoped || captures.iter().any(|c| *c != Some(name)) {
                    return Err(error(format!(
                        "path pattern {pattern:?} uses {{{name}}}, which a namespace pattern of \
                         the grant does not capture"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// The normalized `path` when `writer` may write it: under
/// `<prefix>/<namespace>/` (`namespace`: the writer's own, or the one a
/// cluster-scoped writer's target names), or covered by a grant.
pub fn check<'p>(
    prefix: &str,
    grants: &[PathGrant<'_>],
    writer: Writer<'_>,
    namespace: Option<&str>,
    path: &'p str,
) -> Result<&'p str, PathDenied> {
    let normalized = normalize(path)?;
    let subtree = namespace.map(|ns| subtree(prefix, ns));
    if subtree
        .as_deref()
        .is_some_and(|s| normalized.starts_with(s))
    {
        return Ok(normalized);
    }
    if granted(grants, writer, normalized) {
        return Ok(normalized);
    }
    Err(PathDenied::NotGranted {
        path: path.to_string(),
        subtree,
        namespace: match writer {
            Writer::Namespaced(ns) => Some(ns.to_string()),
            Writer::Cluster => None,
        },
    })
}

/// [`check`] on one of the instance's additional mounts (`grants`: that
/// mount's, `None` when the instance doesn't list `mount`). Such a mount
/// has no subtree: only its grants open a path.
pub fn check_on_mount<'p>(
    mount: &str,
    grants: Option<&[PathGrant<'_>]>,
    writer: Writer<'_>,
    path: &'p str,
) -> Result<&'p str, PathDenied> {
    let Some(grants) = grants else {
        return Err(PathDenied::UnknownMount {
            mount: mount.to_string(),
        });
    };
    let normalized = normalize(path)?;
    if granted(grants, writer, normalized) {
        return Ok(normalized);
    }
    Err(PathDenied::NotGrantedOnMount {
        path: path.to_string(),
        mount: mount.to_string(),
        namespace: match writer {
            Writer::Namespaced(ns) => Some(ns.to_string()),
            Writer::Cluster => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn normalize_strips_leading_slashes_and_refuses_odd_segments() {
        assert_eq!(normalize("/a/b"), Ok("a/b"));
        assert_eq!(normalize("a/b"), Ok("a/b"));
        for bad in ["", "/", "a//b", "a/b/", "a/../b", "..", "a/./b"] {
            assert!(normalize(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_empty_prefix_is_skipped() {
        assert_eq!(subtree("weebo", "team-a"), "weebo/team-a/");
        assert_eq!(subtree("", "team-a"), "team-a/");
        assert_eq!(subtree("/weebo/", "team-a"), "weebo/team-a/");
        assert_eq!(default_path("", "team-a", "ci"), "team-a/ci");
        assert_eq!(default_path("weebo", "team-a", "ci"), "weebo/team-a/ci");
    }

    #[test]
    fn patterns_compare_segment_by_segment() {
        let cases = [
            ("shared/auth", "shared/auth", true),
            ("shared/auth", "shared/auth/x", false),
            ("shared/auth", "shared/authx", false),
            ("+/auth", "shared/auth", true),
            ("+/auth", "auth", false),
            ("+/auth", "a/b/auth", false),
            ("+/auth", "shared/auth/x", false),
            ("a/+/c", "a/b/c", true),
            ("a/+/c", "a/b/d", false),
            ("shared/*", "shared/auth", true),
            ("shared/*", "shared/a/b", true),
            ("shared/*", "shared", false),
            ("shared/*", "sharedx/auth", false),
            ("+/auth/*", "x/auth/y/z", true),
            ("+/auth/*", "x/auth", false),
        ];
        for (pattern, path, expected) in cases {
            assert_eq!(
                pattern_matches(pattern, path, None),
                expected,
                "{pattern} ~ {path}"
            );
        }
    }

    #[test]
    fn validate_refuses_whole_mount_and_malformed_patterns() {
        let ns = s(&["auth"]);
        for bad in [
            "*", "+", "+/*", "+/+", "a/*/b", "/a", "a//b", "a/../b", "a/", "a*", "b+/c",
        ] {
            let paths = s(&[bad]);
            let grant = PathGrant {
                namespaces: &ns,
                cluster_scoped: false,
                paths: &paths,
            };
            assert!(validate(&[grant]).is_err(), "{bad}");
        }
        let paths = s(&["+/auth", "a/+/c/*", "shared/auth"]);
        let good = PathGrant {
            namespaces: &ns,
            cluster_scoped: false,
            paths: &paths,
        };
        assert_eq!(validate(&[good]), Ok(()));
        let no_paths = PathGrant { paths: &[], ..good };
        assert!(validate(&[no_paths]).is_err());
        let both = PathGrant {
            cluster_scoped: true,
            ..good
        };
        assert!(validate(&[both]).is_err());
        let neither = PathGrant {
            namespaces: &[],
            ..good
        };
        assert!(validate(&[neither]).is_err());
    }

    #[test]
    fn a_namespaced_writer_needs_its_subtree_or_a_grant() {
        let ns = s(&["auth", "team-*"]);
        let paths = s(&["shared/auth", "+/creds"]);
        let grants = [PathGrant {
            namespaces: &ns,
            cluster_scoped: false,
            paths: &paths,
        }];
        let ok = |ns: &str, path: &str| {
            check("weebo", &grants, Writer::Namespaced(ns), Some(ns), path).is_ok()
        };
        assert!(ok("other", "weebo/other/ci"));
        assert!(ok("other", "/weebo/other/ci"));
        assert!(!ok("other", "weebo/other-evil/ci"));
        assert!(!ok("other", "weebo/other/../auth/ci"));
        assert!(!ok("other", "shared/auth"));
        assert!(ok("auth", "shared/auth"));
        assert!(ok("team-a", "shared/auth"));
        assert!(ok("auth", "x/creds"));
        assert!(!ok("auth", "shared/auth/x"));
        let err = check(
            "weebo",
            &grants,
            Writer::Namespaced("other"),
            Some("other"),
            "x/y",
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "Vault path \"x/y\" is outside \"weebo/other/\" and no pathGrants entry of the \
             instance allows it for namespace \"other\""
        );
    }

    #[test]
    fn an_invalid_pattern_covers_nothing() {
        let ns = s(&["*"]);
        let paths = s(&["*", "+/+"]);
        let grants = [PathGrant {
            namespaces: &ns,
            cluster_scoped: false,
            paths: &paths,
        }];
        assert!(!granted(&grants, Writer::Namespaced("a"), "x/y"));
    }

    #[test]
    fn a_cluster_writer_needs_a_cluster_scoped_grant() {
        let all = s(&["*"]);
        let paths = s(&["platform/+/credentials"]);
        let namespaced = [PathGrant {
            namespaces: &all,
            cluster_scoped: false,
            paths: &paths,
        }];
        let cluster = [PathGrant {
            namespaces: &[],
            cluster_scoped: true,
            paths: &paths,
        }];
        let path = "platform/x/credentials";
        assert!(check("weebo", &[], Writer::Cluster, Some("ns"), "weebo/ns/ci").is_ok());
        assert!(check("weebo", &[], Writer::Cluster, Some("ns"), path).is_err());
        assert!(check("weebo", &namespaced, Writer::Cluster, None, path).is_err());
        assert!(check("weebo", &cluster, Writer::Cluster, None, path).is_ok());
        assert!(check("weebo", &cluster, Writer::Namespaced("a"), Some("a"), path).is_err());
        assert_eq!(
            check("weebo", &[], Writer::Cluster, None, "x/y")
                .unwrap_err()
                .to_string(),
            "Vault path \"x/y\" names no namespace and no pathGrants entry of the instance \
             allows it for cluster-scoped objects"
        );
    }

    #[test]
    fn a_capture_binds_the_path_to_the_writer_namespace() {
        let ns = s(&["dev-ws-{user}"]);
        let paths = s(&["{user}/forgejo-token"]);
        let grants = [PathGrant {
            namespaces: &ns,
            cluster_scoped: false,
            paths: &paths,
        }];
        assert_eq!(validate(&grants), Ok(()));
        let ok = |ns: &str, path: &str| granted(&grants, Writer::Namespaced(ns), path);
        assert!(ok("dev-ws-max", "max/forgejo-token"));
        assert!(ok("dev-ws-a-b", "a-b/forgejo-token"));
        assert!(!ok("dev-ws-max", "other/forgejo-token"));
        assert!(!ok("dev-ws-max", "max/notes"));
        assert!(!ok("dev-ws-", "/forgejo-token"));
        assert!(!ok("other-max", "max/forgejo-token"));
        assert!(!granted(&grants, Writer::Cluster, "max/forgejo-token"));
        // Suffix, and a capture next to `+` and `*`.
        let ns = s(&["{user}-che"]);
        let paths = s(&["+/{user}/*"]);
        let grants = [PathGrant {
            namespaces: &ns,
            cluster_scoped: false,
            paths: &paths,
        }];
        assert_eq!(validate(&grants), Ok(()));
        assert!(granted(&grants, Writer::Namespaced("max-che"), "x/max/a/b"));
        assert!(!granted(&grants, Writer::Namespaced("max-che"), "x/bob/a"));
    }

    #[test]
    fn validate_refuses_unbound_or_malformed_captures() {
        let refused = [
            (&["team-*"][..], "{user}/t"),
            (&["dev-ws-{user}", "ci"][..], "{user}/t"),
            (&["dev-ws-{team}"][..], "{user}/t"),
            (&["dev-ws-{User}"][..], "x/t"),
            (&["dev-{a}-{b}"][..], "x/t"),
            (&["dev-{a}*"][..], "x/t"),
            (&["dev-{a"][..], "x/t"),
            (&["dev-ws-{user}"][..], "a{user}/t"),
            (&["dev-ws-{user}"][..], "{1}/t"),
            (&["dev-ws-{user}"][..], "{}/t"),
        ];
        for (namespaces, path) in refused {
            let namespaces = s(namespaces);
            let paths = s(&[path]);
            let grant = PathGrant {
                namespaces: &namespaces,
                cluster_scoped: false,
                paths: &paths,
            };
            assert!(validate(&[grant]).is_err(), "{namespaces:?} {path}");
        }
        let paths = s(&["{user}/t"]);
        let cluster = PathGrant {
            namespaces: &[],
            cluster_scoped: true,
            paths: &paths,
        };
        assert!(validate(&[cluster]).is_err());
        // Unbound at runtime too: `ci` captures nothing, `{user}` matches nothing.
        let ns = s(&["dev-ws-{user}", "ci"]);
        let unbound = PathGrant {
            namespaces: &ns,
            cluster_scoped: false,
            paths: &paths,
        };
        assert!(!granted(&[unbound], Writer::Namespaced("ci"), "ci/t"));
        assert!(!granted(&[cluster], Writer::Cluster, "x/t"));
        // A capture counts as a literal: the pattern is bound per writer.
        let ns = s(&["{user}"]);
        let paths = s(&["{user}/*"]);
        let own = PathGrant {
            namespaces: &ns,
            cluster_scoped: false,
            paths: &paths,
        };
        assert_eq!(validate(&[own]), Ok(()));
    }

    #[test]
    fn an_additional_mount_opens_only_its_grants() {
        let ns = s(&["weebo-forgejo"]);
        let paths = s(&["+/forgejo-token"]);
        let grants = [PathGrant {
            namespaces: &ns,
            cluster_scoped: false,
            paths: &paths,
        }];
        let writer = Writer::Namespaced("weebo-forgejo");
        assert_eq!(
            check_on_mount("users", Some(&grants), writer, "/max/forgejo-token"),
            Ok("max/forgejo-token")
        );
        // No subtree: the writer's own namespace folder is not implied.
        for refused in ["weebo-forgejo/x", "max/notes", "max/a/forgejo-token"] {
            let denied = check_on_mount("users", Some(&grants), writer, refused).unwrap_err();
            assert!(
                matches!(denied, PathDenied::NotGrantedOnMount { .. }),
                "{refused}"
            );
        }
        let other = check_on_mount(
            "users",
            Some(&grants),
            Writer::Namespaced("ci"),
            "max/forgejo-token",
        );
        assert!(
            other
                .unwrap_err()
                .to_string()
                .contains("for namespace \"ci\"")
        );
        assert_eq!(
            check_on_mount("orgs-prod", None, writer, "a/b"),
            Err(PathDenied::UnknownMount {
                mount: "orgs-prod".into()
            })
        );
        assert!(matches!(
            check_on_mount("users", Some(&grants), writer, "max/../x"),
            Err(PathDenied::Malformed { .. })
        ));
    }
}
