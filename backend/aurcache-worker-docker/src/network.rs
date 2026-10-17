//! How a spawned build container reaches the AURCache package repository.
//!
//! Builds resolve previously-built packages through the `[repo]` section of the
//! job's `pacman.conf`, which the **server** renders from `AURCACHE_PUBLIC_URL`.
//! The executor therefore cannot rewrite that URL — it can only arrange for the
//! URL to be reachable from inside the container it spawns.
//!
//! That is why the default shares this process's network namespace. In the
//! hybrid image the server, the worker and the repository are one container, so
//! `AURCACHE_PUBLIC_URL` defaults to `http://localhost:8081`; a build container
//! on the default bridge would resolve `localhost` to *itself* and fail to find
//! any AURCache-built dependency, while reporting only a confusing pacman
//! error. Sharing the namespace makes `localhost` mean what the URL intends.
//!
//! The trade is that the build container can then reach anything this process
//! can reach on loopback, including the worker protocol port. That port demands
//! a client certificate the build does not have, but it is exposure worth
//! naming; a deployment that would rather not have it sets
//! `AURCACHE_BUILDER_NETWORK` and a matching `AURCACHE_PUBLIC_URL`.

/// How to attach a spawned build container to the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetworkPlan {
    /// Share this process's own network namespace (`container:<id>`), so
    /// loopback addresses in the repo URL resolve to this container.
    ShareOwnNamespace(String),
    /// Join a named Docker network, for deployments whose repo URL is a
    /// service name rather than loopback.
    JoinNetwork(String),
    /// Share the host's network namespace.
    Host,
    /// Leave Docker's default networking alone.
    Default,
}

impl NetworkPlan {
    /// The value for `HostConfig.network_mode`, if this plan sets one.
    #[must_use]
    pub fn network_mode(&self) -> Option<String> {
        match self {
            Self::ShareOwnNamespace(id) => Some(format!("container:{id}")),
            Self::Host => Some("host".to_string()),
            Self::JoinNetwork(_) | Self::Default => None,
        }
    }

    /// The network to attach via `NetworkingConfig`, if any.
    #[must_use]
    pub fn endpoint_network(&self) -> Option<&str> {
        match self {
            Self::JoinNetwork(name) => Some(name),
            _ => None,
        }
    }
}

/// Resolve the plan from the environment.
///
/// `lookup` reads an environment variable; `own_id` is this container's id
/// (Docker sets `HOSTNAME` to it), or `None` when it cannot be determined —
/// outside a container, for instance, where sharing a namespace is meaningless.
pub fn resolve(lookup: impl Fn(&str) -> Option<String>, own_id: Option<String>) -> NetworkPlan {
    let named = |key: &str| lookup(key).filter(|v| !v.trim().is_empty());

    if let Some(network) = named("AURCACHE_BUILDER_NETWORK") {
        if network == "host" {
            return NetworkPlan::Host;
        }
        return NetworkPlan::JoinNetwork(network);
    }

    // Default: make the server-rendered repo URL resolve the way it was
    // written. Falling back to Docker's default would leave a loopback URL
    // pointing at the build container itself.
    match own_id.filter(|id| !id.trim().is_empty()) {
        Some(id) => NetworkPlan::ShareOwnNamespace(id),
        None => NetworkPlan::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| map.get(key).cloned()
    }

    /// The default has to make a loopback repo URL work, which is what the
    /// hybrid image ships; anything else silently breaks dependency resolution.
    #[test]
    fn defaults_to_sharing_our_own_namespace() {
        let plan = resolve(env(&[]), Some("abc123".to_string()));
        assert_eq!(plan, NetworkPlan::ShareOwnNamespace("abc123".to_string()));
        assert_eq!(plan.network_mode().as_deref(), Some("container:abc123"));
        assert_eq!(plan.endpoint_network(), None);
    }

    #[test]
    fn an_explicit_network_is_joined_instead() {
        let plan = resolve(env(&[("AURCACHE_BUILDER_NETWORK", "aurcache_net")]), None);
        assert_eq!(plan, NetworkPlan::JoinNetwork("aurcache_net".to_string()));
        assert_eq!(plan.network_mode(), None);
        assert_eq!(plan.endpoint_network(), Some("aurcache_net"));
    }

    #[test]
    fn host_is_spelled_as_a_network_name() {
        let plan = resolve(env(&[("AURCACHE_BUILDER_NETWORK", "host")]), None);
        assert_eq!(plan, NetworkPlan::Host);
        assert_eq!(plan.network_mode().as_deref(), Some("host"));
    }

    /// Without a container id there is no namespace to share, and guessing one
    /// would produce an unusable `container:` reference.
    #[test]
    fn falls_back_to_default_networking_outside_a_container() {
        assert_eq!(resolve(env(&[]), None), NetworkPlan::Default);
        assert_eq!(
            resolve(env(&[]), Some("  ".to_string())),
            NetworkPlan::Default
        );
    }

    #[test]
    fn a_blank_override_is_ignored() {
        assert_eq!(
            resolve(env(&[("AURCACHE_BUILDER_NETWORK", "  ")]), Some("x".into())),
            NetworkPlan::ShareOwnNamespace("x".to_string())
        );
    }
}
