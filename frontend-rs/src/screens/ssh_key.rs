//! The SSH key the server clones package sources with.
//!
//! Shown in two places: on the settings page, and in the add dialog as soon as
//! the entry is an SSH remote -- which is the moment someone needs it, since a
//! private repository refuses the clone until the key has been added to it.
//! The settings page also lists the host keys the server trusts, so one
//! trusted on first use can be checked against what its host publishes.

use crate::screens::settings::Section;
use aurcache_client::{DEFAULT_KEY, HostKeyPolicy, KeyOrigin, SshInfo, SshKey, TrustedHosts};
use dioxus::prelude::*;

/// The server's SSH key, read once per mount.
fn use_ssh_info() -> Resource<Result<SshInfo, String>> {
    use_resource(|| async move {
        crate::api::client()?
            .ssh_info()
            .await
            .map_err(|e| e.to_string())
    })
}

/// The settings page's card.
#[component]
pub fn SshKeySection() -> Element {
    let info = use_ssh_info();
    rsx! {
        Section { title: "SSH keys",
            div { class: "py-3 space-y-2",
                p { class: "text-sm opacity-70",
                    "Packages added from a git repository over SSH are cloned with this key. \
                     Add it to each private repository as a deploy key, or to an account \
                     that can read them."
                }
                match &*info.read_unchecked() {
                    None => rsx! { span { class: "loading loading-spinner loading-sm" } },
                    Some(Err(e)) => rsx! { div { class: "alert alert-error text-sm", span { "{e}" } } },
                    Some(Ok(info)) => rsx! { SshKeyView { info: info.clone() } },
                }
            }
        }
    }
}

/// Under the add dialog's git fields, for an SSH remote: the key the clone
/// will offer, so it can be added before the add fails for the want of it.
#[component]
pub fn SshSourceHint() -> Element {
    let info = use_ssh_info();
    let guard = info.read_unchecked();
    // Nothing useful to say until it loads, and a failure here is the
    // settings page's to report: the add itself still works.
    let Some(Ok(info)) = &*guard else {
        return rsx! {};
    };
    let Some(key) = info.key(DEFAULT_KEY) else {
        return rsx! {};
    };
    rsx! {
        div { class: "text-xs space-y-1",
            p { class: "opacity-70",
                "Cloned over SSH with the server's key. A private repository needs it added as a deploy key:"
            }
            KeyView { ssh_key: key.clone() }
        }
    }
}

/// Every key the server holds, how hosts are trusted, and which are.
///
/// A key is labelled with its name only when there is more than one: with a
/// single key, `default` says nothing a reader needs.
#[component]
pub fn SshKeyView(info: SshInfo) -> Element {
    let hosts = match info.host_keys {
        HostKeyPolicy::Pinned => "Only hosts listed in AURCACHE_SSH_KNOWN_HOSTS are trusted.",
        HostKeyPolicy::TrustOnFirstUse => {
            "A host's key is recorded the first time the server connects, and a different key from it later is refused."
        }
    };
    let labelled = info.keys.len() > 1;
    rsx! {
        for named in info.keys {
            div { class: "space-y-1",
                if labelled {
                    div { class: "font-medium text-sm font-mono", "{named.name}" }
                }
                KeyView { ssh_key: named.key }
            }
        }
        p { class: "text-xs opacity-60", "{hosts}" }
        TrustedHostsView { hosts: info.trusted_hosts, policy: info.host_keys }
    }
}

/// The host keys the server trusts, each with the fingerprint to check it
/// by.
#[component]
fn TrustedHostsView(hosts: TrustedHosts, policy: HostKeyPolicy) -> Element {
    let hosts = match hosts {
        TrustedHosts::Listed { hosts } => hosts,
        TrustedHosts::Unreadable { reason } => {
            return rsx! {
                div { class: "alert alert-warning text-sm",
                    span { "The trusted host keys cannot be read, so SSH sources fail: {reason}" }
                }
            };
        }
    };
    if hosts.is_empty() {
        let none = match policy {
            HostKeyPolicy::Pinned => {
                "AURCACHE_SSH_KNOWN_HOSTS lists no host key, so every SSH host is refused."
            }
            HostKeyPolicy::TrustOnFirstUse => "No host key recorded yet.",
        };
        return rsx! { p { class: "text-xs opacity-60", "{none}" } };
    }
    rsx! {
        div { class: "space-y-1",
            div { class: "text-sm font-medium", "Trusted host keys" }
            if policy == HostKeyPolicy::TrustOnFirstUse {
                p { class: "text-xs opacity-60",
                    "Recorded on first contact, with nothing to check them against: compare each fingerprint with the one its host publishes."
                }
            }
            div { class: "overflow-x-auto",
                table { class: "table table-xs",
                    thead {
                        tr {
                            th { "Host" }
                            th { "Type" }
                            th { "Fingerprint" }
                        }
                    }
                    tbody {
                        for (index, host) in hosts.into_iter().enumerate() {
                            tr { key: "{index}",
                                td { class: "font-mono",
                                    match host.names {
                                        Some(names) => rsx! { "{names}" },
                                        None => rsx! { span { class: "opacity-60", "hashed name" } },
                                    }
                                }
                                td { class: "font-mono", "{host.key_type}" }
                                td { class: "font-mono break-all select-all", "{host.fingerprint}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One key: its public half with a copy button, its fingerprint and where it
/// came from; or why it cannot be used.
#[component]
fn KeyView(ssh_key: SshKey) -> Element {
    match ssh_key {
        SshKey::Ready {
            public_key,
            fingerprint,
            origin,
            passphrase_protected,
        } => {
            let origin = match origin {
                KeyOrigin::Generated => "generated by the server",
                KeyOrigin::Provided => "from AURCACHE_GIT_SSH_KEY",
            };
            let protected = if passphrase_protected {
                ", protected by a passphrase"
            } else {
                ""
            };
            rsx! {
                div { class: "flex gap-2 items-start",
                    code { class: "font-mono text-xs break-all bg-base-200 rounded p-2 flex-1 select-all",
                        "{public_key}"
                    }
                    CopyButton { text: public_key }
                }
                p { class: "text-xs opacity-60",
                    span { class: "font-mono", "{fingerprint}" }
                    ", {origin}{protected}."
                }
            }
        }
        SshKey::Unavailable { reason } => rsx! {
            div { class: "alert alert-warning text-sm",
                span { "This SSH key cannot be used, so packages from SSH remotes cannot be fetched: {reason}" }
            }
        },
    }
}

#[component]
fn CopyButton(text: String) -> Element {
    let mut copied = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);
    rsx! {
        button {
            class: "btn btn-xs",
            r#type: "button",
            disabled: copied(),
            onclick: move |_| {
                let text = text.clone();
                async move {
                    match crate::clipboard::copy_text(&text).await {
                        Ok(()) => {
                            copied.set(true);
                            error.set(None);
                            gloo_timers::future::TimeoutFuture::new(2000).await;
                            copied.set(false);
                        }
                        Err(e) => error.set(Some(e.message("the key"))),
                    }
                }
            },
            if copied() { "Copied" } else { "Copy" }
        }
        if let Some(e) = error() {
            span { class: "text-xs text-error", "{e}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SshKeyView, SshKeyViewProps};
    use aurcache_client::{
        DEFAULT_KEY, HostKeyPolicy, KeyOrigin, NamedKey, SshInfo, SshKey, TrustedHost, TrustedHosts,
    };
    use dioxus::prelude::*;

    fn render(keys: Vec<NamedKey>, host_keys: HostKeyPolicy) -> String {
        render_with_hosts(keys, host_keys, TrustedHosts::Listed { hosts: Vec::new() })
    }

    fn render_with_hosts(
        keys: Vec<NamedKey>,
        host_keys: HostKeyPolicy,
        trusted_hosts: TrustedHosts,
    ) -> String {
        let info = SshInfo {
            keys,
            host_keys,
            trusted_hosts,
        };
        let mut dom = VirtualDom::new_with_props(SshKeyView, SshKeyViewProps { info });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn ready(name: &str, public_key: &str) -> NamedKey {
        NamedKey {
            name: name.to_string(),
            key: SshKey::Ready {
                public_key: public_key.to_string(),
                fingerprint: "SHA256:abc".to_string(),
                origin: KeyOrigin::Generated,
                passphrase_protected: false,
            },
        }
    }

    #[test]
    fn a_ready_key_shows_the_key_its_fingerprint_and_the_host_policy() {
        let html = render(
            vec![ready(DEFAULT_KEY, "ssh-ed25519 AAAAkey aurcache-server")],
            HostKeyPolicy::TrustOnFirstUse,
        );
        assert!(
            html.contains("ssh-ed25519 AAAAkey aurcache-server"),
            "{html}"
        );
        assert!(html.contains("SHA256:abc"), "{html}");
        assert!(html.contains("generated by the server"), "{html}");
        assert!(html.contains("recorded the first time"), "{html}");
        // One key needs no name.
        assert!(!html.contains(">default<"), "{html}");
    }

    #[test]
    fn several_keys_are_told_apart_by_name() {
        let html = render(
            vec![
                ready(DEFAULT_KEY, "ssh-ed25519 AAAAone"),
                ready("org-repo", "ssh-ed25519 AAAAtwo"),
            ],
            HostKeyPolicy::Pinned,
        );
        assert!(html.contains(">default<"), "{html}");
        assert!(html.contains(">org-repo<"), "{html}");
        assert!(html.contains("AAAAtwo"), "{html}");
    }

    #[test]
    fn a_missing_key_says_why() {
        let html = render(
            vec![NamedKey {
                name: DEFAULT_KEY.to_string(),
                key: SshKey::Unavailable {
                    reason: "AURCACHE_GIT_SSH_KEY points at /nope, which does not exist"
                        .to_string(),
                },
            }],
            HostKeyPolicy::Pinned,
        );
        assert!(html.contains("points at /nope"), "{html}");
        assert!(!html.contains("Copy"), "{html}");
    }

    /// A host trusted on first use is listed with its fingerprint, and the
    /// page says to check it; a hashed entry is shown as one.
    #[test]
    fn trusted_host_keys_are_listed_with_their_fingerprints() {
        let host = |names: Option<&str>, fingerprint: &str| TrustedHost {
            names: names.map(str::to_string),
            key_type: "ssh-ed25519".to_string(),
            fingerprint: fingerprint.to_string(),
        };
        let html = render_with_hosts(
            vec![ready(DEFAULT_KEY, "ssh-ed25519 AAAAkey")],
            HostKeyPolicy::TrustOnFirstUse,
            TrustedHosts::Listed {
                hosts: vec![
                    host(Some("[git.example.com]:2222"), "SHA256:first"),
                    host(None, "SHA256:second"),
                ],
            },
        );
        assert!(html.contains("[git.example.com]:2222"), "{html}");
        assert!(html.contains("SHA256:first"), "{html}");
        assert!(html.contains("hashed name"), "{html}");
        assert!(html.contains("compare each fingerprint"), "{html}");
    }

    #[test]
    fn unreadable_host_keys_say_why() {
        let html = render_with_hosts(
            vec![ready(DEFAULT_KEY, "ssh-ed25519 AAAAkey")],
            HostKeyPolicy::Pinned,
            TrustedHosts::Unreadable {
                reason: "reading /etc/aurcache/known_hosts: permission denied".to_string(),
            },
        );
        assert!(html.contains("permission denied"), "{html}");
    }
}
