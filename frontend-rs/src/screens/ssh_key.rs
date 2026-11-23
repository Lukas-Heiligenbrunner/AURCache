//! The server's SSH key for cloning package sources.
//!
//! Shown in two places: on the settings page, and in the add dialog as soon as
//! the entry is an SSH remote -- which is the moment someone needs it, since a
//! private repository refuses the clone until the key has been added to it.

use crate::screens::settings::Section;
use aurcache_client::SshKey;
use dioxus::prelude::*;

/// The server's SSH key, read once per mount.
fn use_ssh_key() -> Resource<Result<SshKey, String>> {
    use_resource(|| async move {
        crate::api::client()?
            .ssh_key()
            .await
            .map_err(|e| e.to_string())
    })
}

/// The settings page's card.
#[component]
pub fn SshKeySection() -> Element {
    let key = use_ssh_key();
    rsx! {
        Section { title: "SSH key",
            div { class: "py-3 space-y-2",
                p { class: "text-sm opacity-70",
                    "Packages added from a git repository over SSH are cloned with this key. \
                     Add it to each private repository as a deploy key, or to an account \
                     that can read them."
                }
                match &*key.read_unchecked() {
                    None => rsx! { span { class: "loading loading-spinner loading-sm" } },
                    Some(Err(e)) => rsx! { div { class: "alert alert-error text-sm", span { "{e}" } } },
                    Some(Ok(key)) => rsx! { KeyView { ssh_key: key.clone() } },
                }
            }
        }
    }
}

/// Under the add dialog's git fields, for an SSH remote: the key that the
/// clone will offer, to add before the add fails for the want of it.
#[component]
pub fn SshSourceHint() -> Element {
    let key = use_ssh_key();
    let guard = key.read_unchecked();
    // Nothing useful to say until it loads, and a failure here is the
    // settings page's to report: the add itself still works.
    let Some(Ok(key)) = &*guard else {
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

/// The key's public half, selected in one click, and its fingerprint; or why
/// it cannot be used.
#[component]
fn KeyView(ssh_key: SshKey) -> Element {
    match ssh_key {
        SshKey::Ready {
            public_key,
            fingerprint,
        } => rsx! {
            code { class: "block font-mono text-xs break-all bg-base-200 rounded p-2 select-all",
                "{public_key}"
            }
            p { class: "text-xs opacity-60 font-mono", "{fingerprint}" }
        },
        SshKey::Unavailable { reason } => rsx! {
            div { class: "alert alert-warning text-sm",
                span { "The server's SSH key cannot be used, so packages from SSH remotes cannot be fetched: {reason}" }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyView, KeyViewProps};
    use aurcache_client::SshKey;
    use dioxus::prelude::*;

    fn render(ssh_key: SshKey) -> String {
        let mut dom = VirtualDom::new_with_props(KeyView, KeyViewProps { ssh_key });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn a_ready_key_shows_the_key_and_its_fingerprint() {
        let html = render(SshKey::Ready {
            public_key: "ssh-ed25519 AAAAkey aurcache-server".to_string(),
            fingerprint: "SHA256:abc".to_string(),
        });
        assert!(
            html.contains("ssh-ed25519 AAAAkey aurcache-server"),
            "{html}"
        );
        assert!(html.contains("SHA256:abc"), "{html}");
    }

    #[test]
    fn a_missing_key_says_why() {
        let html = render(SshKey::Unavailable {
            reason: "AURCACHE_GIT_SSH_KEY points at /nope, which does not exist".to_string(),
        });
        assert!(html.contains("points at /nope"), "{html}");
    }
}
