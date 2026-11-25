//! phoenix on Desktop Phoenix: the access gate, the HTTP transport hooks, the
//! Sunbird pages in User Settings, message accessories and the tray.

#![cfg(windows)]

mod accessories;
mod badges;
mod context_menu;
mod discord;
mod fetch;
mod gpui;
mod js;
mod modal;
mod settings;
mod staff;

use std::{
   ffi::c_void,
   path::{
      Path,
      PathBuf,
   },
   sync::{
      Arc,
      Mutex,
      PoisonError,
   },
   time::Duration,
};

use phoenix_hook::image::Image;
use phoenix_runtime::{
   Error,
   Notice,
   Platform,
   Plugin,
   Started,
   discord::{
      Authorization,
      DiscordError,
   },
   fetch::{
      FetchError,
      Request,
      Response,
   },
   remote::{
      self,
      Done,
   },
   store::{
      Rows,
      StoreError,
   },
};
use phoenix_sigs::registry::GatewayRegistry;
use rquickjs::{
   Ctx,
   Object,
};

struct Desktop;

/// A `POST /ui/choose` or `POST /ui/sheet` body.
#[derive(serde::Deserialize)]
struct DialogRequest {
   title:     Option<String>,
   message:   Option<String>,
   text:      Option<String>,
   monospace: Option<bool>,
   highlight: Option<bool>,
   options:   Option<Vec<DialogItem>>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum DialogItem {
   Label(String),
   Item {
      label:  String,
      #[serde(default)]
      danger: bool,
   },
}

#[derive(serde::Serialize)]
struct DialogAnswer {
   index: i32,
}

/// The target and its swapped gateway registry, once [`prepare`] hooked them.
static PREPARED: Mutex<Option<(Image<'static>, &'static GatewayRegistry)>> = Mutex::new(None);

/// Hooks `Discord.exe` from `DllMain`, before its own code runs.
///
/// Desktop Phoenix settles on the access gate in its first frames and stops
/// reading the gateway registry and HTTP vtables once started, so hooks swapped
/// later from [`start`]'s thread never run. [`start`] picks up what this
/// hooked.
pub fn prepare() -> Result<(), Error> {
   phoenix_runtime::set_platform(&Desktop);
   let image = phoenix_runtime::target()?;
   open_access_gate(&image);
   staff::prepare(&image);
   settings::install(&image);
   modal::install(&image);
   accessories::install(&image);
   context_menu::install(&image);
   badges::install(&image);
   let registry = phoenix_runtime::hook(&image)?;
   discord::install(&image);
   accessories::install_optimistic_delete(&image);
   phoenix_sigs::desktop::release_call_index();
   *PREPARED.lock().unwrap_or_else(PoisonError::into_inner) = Some((image, registry));
   Ok(())
}

/// Starts phoenix inside Desktop Phoenix, on what [`prepare`] hooked.
pub fn start(home: PathBuf, bundled: Vec<Plugin>) -> Result<Started, Error> {
   let prepared = PREPARED
      .lock()
      .unwrap_or_else(PoisonError::into_inner)
      .take();
   if let Some((image, registry)) = prepared {
      return phoenix_runtime::start_hooked(&Desktop, image, registry, home, bundled);
   }
   let image = phoenix_runtime::target()?;
   let registry = phoenix_runtime::hook(&image)?;
   discord::install(&image);
   phoenix_runtime::start_hooked(&Desktop, image, registry, home, bundled)
}

/// Desktop Phoenix shows "This app isn't available for your account" from
/// `libdiscord_ops_surfaces::client_access::ClientAccessGate`. One function
/// reads it (1 blocked, 2 no gate, also what a missing value reads as), and the
/// gate is only considered while it is not 2: its one small caller is
/// `signed_in && access(..) != 2`. This makes it return 2. Discord settles on
/// the gate within its first frames, so the loader calls this from `DllMain`,
/// before Discord's own code.
fn open_access_gate(image: &Image<'static>) {
   /// `mov al, 2; ret`
   const ALLOWED: [u8; 3] = [0xB0, 0x02, 0xC3];

   let access = match phoenix_sigs::desktop::access_gate(image) {
      Ok(access) => access,
      Err(err) => {
         log::warn!("access gate: {err}, leaving it");
         return;
      },
   };
   // SAFETY: code in Discord.exe, written from `DllMain` before Discord's own
   // code runs.
   match unsafe { image.write_code(access, &ALLOWED) } {
      Ok(()) => log::info!("access gate: open"),
      Err(err) => log::error!("access gate: {err}"),
   }
}

impl Platform for Desktop {
   fn send(&self, request: &Request) -> Result<Response, FetchError> {
      fetch::send(request)
   }

   fn query(&self, database: &Path, sql: &str, args: &[String]) -> Result<Rows, StoreError> {
      phoenix_runtime::store::sqlite(database, sql, args)
   }

   fn discord(
      &self,
      method: &str,
      path: &str,
      headers: Vec<(String, String)>,
      body: Vec<u8>,
   ) -> Result<Response, DiscordError> {
      discord::send(method, path, headers, body)
   }

   fn authorize(&self, request: &Authorization) -> Result<String, DiscordError> {
      discord::authorize(request)
   }

   fn init(&self, ctx: &Ctx<'_>) -> rquickjs::Result<()> {
      ctx.store_userdata(js::Desktop::default())
         .map_err(|err| rquickjs::Exception::throw_internal(ctx, &err.to_string()))?;
      Ok(())
   }

   fn api<'js>(
      &self,
      ctx: &Ctx<'js>,
      plugin: &str,
      api: &Object<'js>,
      messages: &Object<'js>,
   ) -> rquickjs::Result<()> {
      js::ui::desktop_patch_api(ctx, plugin, api, messages)
   }

   fn console<'js>(&self, ctx: &Ctx<'js>, phoenix: &Object<'js>) -> rquickjs::Result<()> {
      phoenix.set("native", js::native::object(ctx)?)
   }

   fn unload(&self, ctx: &Ctx<'_>, plugin: &str) {
      js::unload(ctx, plugin);
   }

   fn staff(&self, image: &'static Image<'static>) {
      staff::start(image);
   }

   fn gateway(&self, store: *mut c_void) {
      // SAFETY: every registry handler takes the store state first, and this
      // runs inside one.
      unsafe { staff::apply(store) };
   }

   fn started(&self, first_run: bool, plugins: &[String]) {
      let Some(state) = phoenix_runtime::state() else {
         return;
      };
      let _ = std::fs::create_dir_all(state.plugins_dir());
      remote::set_flag(true);
      remote::ensure_started();
      phoenix_ui_desktop::start_tray(phoenix_ui_desktop::TrayConfig {
         dashboard_url:    format!("http://{}/ui", remote::bound_addr()),
         plugins_dir:      state.plugins_dir(),
         on_reload_all:    Some(Arc::new(phoenix_runtime::reload_all)),
         on_check_updates: Some(Arc::new(phoenix_runtime::update::check)),
      });
      phoenix_ui_desktop::notify(
         "Sunbird ready",
         Some("Open User Settings -> Sunbird, or press Ctrl+Shift+I for DevTools"),
      );
      if first_run && plugins.is_empty() {
         phoenix_ui_desktop::open_settings();
      }
      phoenix_runtime::update::check();
   }

   fn refresh(&self) {}

   fn toast(&self, message: &str) {
      phoenix_ui_desktop::toast(message);
   }

   fn notify(&self, notice: Notice<'_>) {
      match notice {
         Notice::UpdateAvailable(update) => {
            let body = format!(
               "Updated plugins: {}, click to open Settings",
               update.plugins.join(", ")
            );
            phoenix_ui_desktop::notify("Sunbird update available", Some(&body));
         },
         Notice::PluginsUpdated(names) if !names.is_empty() => {
            let body = format!("Reloaded: {}", names.join(", "));
            phoenix_ui_desktop::notify("Plugins updated", Some(&body));
         },
         Notice::PluginsUpdated(_) => {},
      }
   }

   fn route(&self, request: &remote::Request) -> Option<remote::Reply> {
      let html = || {
         (
            "200 OK",
            "text/html; charset=utf-8",
            phoenix_ui_desktop::DASHBOARD_HTML.as_bytes().to_vec(),
         )
      };
      match (request.method.as_str(), request.path.as_str()) {
         ("GET", "/ui") => Some(html()),
         ("GET", "/") if request.accept.contains("text/html") => Some(html()),
         ("POST", "/ui/devtools") => {
            phoenix_ui_desktop::open_devtools();
            Some(remote::ok_json(&Done::Started))
         },
         ("POST", "/ui/choose") => Some(dialog(&request.body, false)),
         ("POST", "/ui/sheet") => Some(dialog(&request.body, true)),
         _ => None,
      }
   }

   fn routes(&self) -> &'static str {
      ", GET /ui, POST /ui/devtools, POST /ui/{choose,sheet}"
   }
}

/// Opens a native choose or sheet dialog for the dashboard and blocks this
/// request's thread until it is answered.
fn dialog(body: &[u8], sheet: bool) -> remote::Reply {
   let Ok(request) = serde_json::from_slice::<DialogRequest>(body) else {
      return remote::err("400 Bad Request", "expected a JSON dialog request");
   };
   let options = request
      .options
      .unwrap_or_default()
      .into_iter()
      .map(|item| {
         match item {
            DialogItem::Label(label) => {
               modal::ChoiceOption {
                  label,
                  danger: false,
               }
            },
            DialogItem::Item { label, danger } => modal::ChoiceOption { label, danger },
         }
      })
      .collect::<Vec<_>>();
   let content = if sheet {
      request.text.map(|text| {
         modal::ModalSection::Code {
            title: None,
            text,
            monospace: request.monospace.unwrap_or_default(),
            highlight: request.highlight.unwrap_or_default(),
         }
      })
   } else {
      request
         .message
         .map(|text| modal::ModalSection::Text { title: None, text })
   };
   if content.is_none() && options.is_empty() {
      return remote::err(
         "400 Bad Request",
         if sheet {
            "a sheet needs text"
         } else {
            "a choose dialog needs a message or options"
         },
      );
   }
   let title = request.title.unwrap_or_else(|| "Sunbird".to_owned());
   let choice = modal::open_choice(title, content, options);
   let index = choice.wait(Duration::from_secs(120)).unwrap_or(-1);
   if index == -1 {
      modal::dismiss_choice(&choice);
   }
   remote::ok_json(&DialogAnswer { index })
}
