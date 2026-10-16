//! HTML via one long-lived WebKitGTK view.
//!
//! Files can be hostile and a preview must not phone home: JavaScript is off, file://
//! pages can't read other files, navigation away from file:// is refused, and the
//! ephemeral network session is pointed at a dead proxy so remote subresources fail.
//!
//! TODO: replace the dead-proxy trick with a compiled UserContentFilter.

use super::Page;
use gtk::prelude::*;
use std::path::Path;
use webkit6::prelude::*;

/// Discard port: connections are refused immediately.
const DEAD_PROXY: &str = "http://127.0.0.1:9";

pub struct WebPage {
    view: webkit6::WebView,
}

impl Page for WebPage {
    const NAME: &'static str = "web";

    fn build() -> Self {
        crate::hit!("build");
        let session = webkit6::NetworkSession::new_ephemeral();
        let proxy = webkit6::NetworkProxySettings::new(Some(DEAD_PROXY), &[]);
        session.set_proxy_settings(webkit6::NetworkProxyMode::Custom, Some(&proxy));

        let settings = webkit6::Settings::builder()
            .enable_javascript(false)
            .enable_javascript_markup(false)
            .allow_file_access_from_file_urls(false)
            .allow_universal_access_from_file_urls(false)
            .enable_webgl(false)
            .enable_page_cache(false)
            .build();

        let view = webkit6::WebView::builder()
            .network_session(&session)
            .settings(&settings)
            .hexpand(true)
            .vexpand(true)
            .build();

        view.connect_decide_policy(|_, decision, kind| {
            use webkit6::PolicyDecisionType as T;
            if !matches!(kind, T::NavigationAction | T::NewWindowAction) {
                return false;
            }
            let uri = decision
                .downcast_ref::<webkit6::NavigationPolicyDecision>()
                .and_then(|d| d.navigation_action())
                .and_then(|action| action.request())
                .and_then(|request| request.uri());
            let local = uri.is_some_and(|u| u.starts_with("file://") || u == "about:blank");
            if !local || kind == T::NewWindowAction {
                decision.ignore();
                return true;
            }
            false
        });

        Self { view }
    }

    fn widget(&self) -> gtk::Widget {
        self.view.clone().upcast()
    }
}

impl WebPage {
    pub fn load(&self, path: &Path) {
        self.view.load_uri(&gio::File::for_path(path).uri());
    }
}
