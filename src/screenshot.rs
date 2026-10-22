#![allow(dead_code, unused_variables)]

use cosmic::cosmic_config::CosmicConfigEntry;
use cosmic::iced::clipboard::mime::AsMimeTypes;
use cosmic::iced::keyboard::Key;
use cosmic::iced::keyboard::key::Named;
use cosmic::iced::platform_specific::shell::commands::layer_surface::destroy_layer_surface;
use cosmic::iced::runtime::clipboard;
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::{
    IcedOutput, SctkLayerSurfaceSettings,
};
use cosmic::iced::{Length, Limits, window};
use cosmic::widget::space;
use cosmic_client_toolkit::sctk::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
use futures::stream::{FuturesUnordered, StreamExt};
use image::RgbaImage;
use rustix::fd::AsFd;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc::Sender;

use wayland_client::protocol::wl_output::WlOutput;
use zbus::zvariant;

use crate::app::{CosmicPortal, OutputState};
use crate::config::screenshot::ImageSaveLocation;
use crate::config::{self};
use crate::screenshot_translate::{self as translate, LogicalRect, Region};
use crate::wayland::{CaptureSource, ShmImage, WaylandHelper};
use crate::widget::keyboard_wrapper::KeyboardWrapper;
use crate::widget::rectangle_selection::DragState;
use crate::widget::screenshot::TranslateControls;
use crate::{PortalResponse, fl, subscription};
use cosmic_screenshot_engine::lang;

#[derive(Clone, Debug)]
pub struct ScreenshotImage {
    pub rgba: RgbaImage,
    pub handle: cosmic::widget::image::Handle,
}

impl ScreenshotImage {
    fn new<T: AsFd>(img: ShmImage<T>) -> anyhow::Result<Self> {
        let rgba = img.image_transformed()?;
        let handle = cosmic::widget::image::Handle::from_rgba(
            rgba.width(),
            rgba.height(),
            rgba.clone().into_vec(),
        );
        Ok(Self { rgba, handle })
    }

    pub fn width(&self) -> u32 {
        self.rgba.width()
    }

    pub fn height(&self) -> u32 {
        self.rgba.height()
    }
}

#[derive(zvariant::DeserializeDict, zvariant::Type, Clone, Debug)]
#[zvariant(signature = "a{sv}")]
pub struct ScreenshotOptions {
    modal: Option<bool>,
    interactive: Option<bool>,
    /// Custom value allowing the client to request the screenshot destination to be chosen.
    ///
    /// Defaults to false
    choose_destination: Option<bool>,
}

#[derive(zvariant::SerializeDict, zvariant::Type)]
#[zvariant(signature = "a{sv}")]
pub struct ScreenshotResult {
    uri: String,
}

struct ScreenshotBytes {
    bytes: Vec<u8>,
}

impl ScreenshotBytes {
    fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

impl AsMimeTypes for ScreenshotBytes {
    fn available(&self) -> std::borrow::Cow<'static, [String]> {
        Cow::Owned(vec!["image/png".to_string()])
    }

    fn as_bytes(&self, mime_type: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
        Some(Cow::Owned(self.bytes.clone()))
    }
}

#[derive(zvariant::SerializeDict, zvariant::Type)]
#[zvariant(signature = "a{sv}")]
struct PickColorResult {
    color: (f64, f64, f64), // (ddd)
}

/// Logical Size and Position of a rectangle
#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    fn intersect(&self, other: Rect) -> Option<Rect> {
        let left = self.left.max(other.left);
        let top = self.top.max(other.top);
        let right = self.right.min(other.right);
        let bottom = self.bottom.min(other.bottom);
        if left < right && top < bottom {
            Some(Rect {
                left,
                top,
                right,
                bottom,
            })
        } else {
            None
        }
    }

    fn translate(&self, x: i32, y: i32) -> Rect {
        Rect {
            left: self.left + x,
            top: self.top + y,
            right: self.right + x,
            bottom: self.bottom + y,
        }
    }

    fn width(&self) -> i32 {
        self.right - self.left
    }

    fn height(&self) -> i32 {
        self.bottom - self.top
    }

    pub fn dimensions(self) -> Option<RectDimension> {
        let width = NonZeroU32::new((self.width()).unsigned_abs())?;
        let height = NonZeroU32::new((self.height()).unsigned_abs())?;
        Some(RectDimension { width, height })
    }
}

#[derive(Clone, Copy)]
pub struct RectDimension {
    width: NonZeroU32,
    height: NonZeroU32,
}

pub struct Screenshot {
    wayland_helper: WaylandHelper,
    tx: Sender<subscription::Event>,
}

impl Screenshot {
    pub fn new(wayland_helper: WaylandHelper, tx: Sender<subscription::Event>) -> Self {
        Self { wayland_helper, tx }
    }

    async fn interactive_toplevel_images(
        &self,
        outputs: &[Output],
    ) -> anyhow::Result<HashMap<String, Vec<ScreenshotImage>>> {
        let wayland_helper = self.wayland_helper.clone();
        Ok(outputs
            .iter()
            .map(move |Output { output, name, .. }| {
                let wayland_helper = wayland_helper.clone();
                async move {
                    let frame = wayland_helper
                        .capture_output_toplevels_shm(output, false)
                        .filter_map(|img| async { ScreenshotImage::new(img).ok() })
                        .collect()
                        .await;
                    (name.clone(), frame)
                }
            })
            .collect::<FuturesUnordered<_>>()
            .collect::<HashMap<String, _>>()
            .await)
    }

    async fn interactive_output_images(
        &self,
        outputs: &[Output],
        app_id: &str,
    ) -> anyhow::Result<HashMap<String, ScreenshotImage>> {
        // collect screenshots from each output

        let wayland_helper = self.wayland_helper.clone();

        let mut map = HashMap::with_capacity(outputs.len());
        for Output {
            output,
            logical_position: (output_x, output_y),
            name,
            ..
        } in outputs
        {
            let frame = wayland_helper
                .capture_source_shm(CaptureSource::Output(output.clone()), false)
                .await
                .ok_or_else(|| anyhow::anyhow!("shm screencopy failed"))?;
            map.insert(name.clone(), ScreenshotImage::new(frame)?);
        }

        Ok(map)
    }

    pub fn save_rgba(img: &RgbaImage, path: Option<&Path>) -> anyhow::Result<Vec<u8>> {
        // Write to the buffer first since the image data will always be copied to the clipboard.
        // This skips encoding the PNG twice.
        let mut buffer = Vec::new();
        write_png(&mut buffer, img)?;

        if let Some(path) = path {
            std::fs::write(path, &buffer)?;
        }

        Ok(buffer)
    }

    pub fn get_img_path(location: ImageSaveLocation) -> Option<PathBuf> {
        let mut path = match location {
            ImageSaveLocation::Pictures => {
                // First check for XDG_SCREENSHOTS_DIR environment variable
                std::env::var_os("XDG_SCREENSHOTS_DIR")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .or_else(|| {
                        // Fall back to XDG_PICTURES_DIR/Screenshots or ~/Pictures/Screenshots
                        dirs::picture_dir()
                            .or_else(|| dirs::home_dir().map(|h| h.join("Pictures")))
                            .map(|p| p.join("Screenshots"))
                    })
            }
            ImageSaveLocation::Documents => {
                dirs::document_dir().or_else(|| dirs::home_dir().map(|h| h.join("Documents")))
            }
            ImageSaveLocation::Clipboard => None,
        }?;

        // Ensure the directory exists
        if let Err(err) = std::fs::create_dir_all(&path) {
            tracing::error!("Failed to create screenshot directory {:?}: {}", path, err);
            return None;
        }

        let name = jiff::Zoned::now()
            .strftime("Screenshot_%Y-%m-%d_%H-%M-%S.png")
            .to_string();
        path.push(name);

        Some(path)
    }

    async fn screenshot_inner(&self, outputs: &[Output], app_id: &str) -> anyhow::Result<PathBuf> {
        let wayland_helper = self.wayland_helper.clone();

        let mut bounds_opt: Option<Rect> = None;
        let mut frames = Vec::with_capacity(outputs.len());
        for Output {
            output,
            logical_position: (output_x, output_y),
            logical_size: (output_w, output_h),
            ..
        } in outputs
        {
            let frame = wayland_helper
                .capture_source_shm(CaptureSource::Output(output.clone()), false)
                .await
                .ok_or_else(|| anyhow::anyhow!("shm screencopy failed"))?;
            let frame_image = frame.image_transformed()?;
            let rect = Rect {
                left: *output_x,
                top: *output_y,
                right: output_x.saturating_add(*output_w),
                bottom: output_y.saturating_add(*output_h),
            };
            bounds_opt = Some(match bounds_opt.take() {
                Some(bounds) => Rect {
                    left: bounds.left.min(rect.left),
                    top: bounds.top.min(rect.top),
                    right: bounds.right.max(rect.right),
                    bottom: bounds.bottom.max(rect.bottom),
                },
                None => rect,
            });
            frames.push((frame_image, rect));
        }

        let (file, path) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let image = combined_image(bounds_opt.unwrap_or_default(), frames);

            let mut file = tempfile::Builder::new()
                .prefix("screenshot-")
                .suffix(".png")
                .tempfile()?;
            {
                write_png(&mut file, &image)?;
            }
            Ok(file.keep()?)
        })
        .await??;

        Ok(path)
    }
}

fn combined_image(bounds: Rect, frames: Vec<(RgbaImage, Rect)>) -> RgbaImage {
    // If we have only one image, crop without scaling
    if frames.len() == 1 {
        let (frame_image, rect) = &frames[0];

        // TODO Don't have explicit scale factor; how to ensure pixel perfect scaling?
        let width_scale = frame_image.width() as f64 / rect.width() as f64;
        let height_scale = frame_image.height() as f64 / rect.height() as f64;

        let width = (bounds.width() as f64 * width_scale).max(0.) as u32;
        let height = (bounds.height() as f64 * height_scale).max(0.) as u32;
        let x = ((bounds.left - rect.left) as f64 * width_scale).max(0.) as u32;
        let y = ((bounds.top - rect.top) as f64 * height_scale).max(0.) as u32;

        return image::imageops::crop_imm(frame_image, x, y, width, height).to_image();
    }

    let width = bounds
        .right
        .saturating_sub(bounds.left)
        .try_into()
        .unwrap_or_default();
    let height = bounds
        .bottom
        .saturating_sub(bounds.top)
        .try_into()
        .unwrap_or_default();
    let mut image = image::RgbaImage::new(width, height);
    for (mut frame_image, rect) in frames {
        let width = rect.width() as u32;
        let height = rect.height() as u32;
        if frame_image.dimensions() != (width, height) {
            frame_image = image::imageops::resize(
                &frame_image,
                width,
                height,
                image::imageops::FilterType::Lanczos3,
            );
        };
        let x = i64::from(rect.left) - i64::from(bounds.left);
        let y = i64::from(rect.top) - i64::from(bounds.top);
        image::imageops::overlay(&mut image, &frame_image, x, y);
    }
    image
}

fn write_png<W: io::Write>(w: W, image: &RgbaImage) -> Result<(), png::EncodingError> {
    let mut encoder = png::Encoder::new(w, image.width(), image.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(image.as_raw())
}

#[derive(Debug, Clone)]
pub enum Msg {
    Capture,
    CaptureWithLocation(ImageSaveLocation),
    Cancel,
    Choice(Choice),
    OutputChanged(WlOutput),
    WindowChosen(String, usize),
    Location(usize),
    /// Translate the selected area of this dialog's output, in place.
    Translate {
        output: String,
    },
    /// A new target language was picked for the next translation.
    TargetLanguage(usize),
    /// The background task finished: the repainted region, or one line of why.
    Translated(Result<translate::Translated, String>),
}

#[derive(Debug, Clone)]
pub enum Choice {
    Output(String),
    Rectangle(Rect, DragState),
    Window(String, Option<usize>),
}

impl From<&Choice> for config::screenshot::Choice {
    // Using a reference here to avoid requiring a temporary `Choice` that's only consumed
    fn from(value: &Choice) -> Self {
        match value {
            Choice::Window(..) => config::screenshot::Choice::Window,
            Choice::Rectangle(..) => config::screenshot::Choice::Rectangle,
            Choice::Output(output) => config::screenshot::Choice::Output(Some(output.clone())),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub enum Action {
    #[default]
    ReturnPath,
    SaveToClipboard,
    SaveToPictures,
    SaveToDocuments,
    ChooseFolder, // TODO use document portal to choose folder
    Choice(Choice),
}

#[derive(Clone, Debug)]
pub struct Args {
    pub handle: zvariant::ObjectPath<'static>,
    pub app_id: String,
    pub parent_window: String,
    pub options: ScreenshotOptions,
    pub output_images: HashMap<String, ScreenshotImage>,
    pub toplevel_images: HashMap<String, Vec<ScreenshotImage>>,
    pub tx: Sender<PortalResponse<ScreenshotResult>>,
    pub choice: Choice,
    pub location: ImageSaveLocation,
    pub action: Action,
    /// The untouched capture of each output. Translate always repaints these,
    /// so translating twice (or into a second language) cannot compound two
    /// overlays on the same glyphs.
    pub pristine: HashMap<String, RgbaImage>,
    /// Index into [`lang::LANGS`]: the target of the next translation.
    pub target_index: usize,
    /// A translation is running. The dialog stays usable; the button says so.
    pub translating: bool,
    /// One line under the toolbar: the result, or the reason it failed.
    pub status: Option<String>,
}

impl Args {
    /// The chosen target language. `LANGS` is non-empty and the index is only
    /// ever set from a valid dropdown selection.
    fn target(&self) -> &'static lang::LangInfo {
        lang::LANGS
            .get(self.target_index)
            .unwrap_or(&lang::LANGS[0])
    }
}

/// The target the dialog opens with: the session's language when it is one of
/// the 23 targets, English otherwise. The owner can change it in the toolbar.
fn target_index() -> usize {
    lang::from_locale()
        .and_then(|info| lang::LANGS.iter().position(|row| row.code == info.code))
        .unwrap_or_else(|| {
            lang::LANGS
                .iter()
                .position(|row| row.code == "en")
                .unwrap_or(0)
        })
}

struct Output {
    output: WlOutput,
    logical_position: (i32, i32),
    logical_size: (i32, i32),
    name: String,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl Screenshot {
    async fn screenshot(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: zvariant::ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        options: ScreenshotOptions,
    ) -> PortalResponse<ScreenshotResult> {
        // connection.object_server().at(&handle, Request);

        // The screenshot handler is created when the portal is launched, but requests are
        // handled on demand. The handler does not store extra state such as a reference to the
        // portal. Storing a copy of the config is unideal because it would remain out of date.
        //
        // The most straightforward solution is to load the screenshot config here
        let config = config::Config::load().0.screenshot;

        // TODO create handle, show dialog
        let mut outputs = Vec::new();
        for output in self.wayland_helper.outputs() {
            let Some(info) = self.wayland_helper.output_info(&output) else {
                tracing::warn!("Output {:?} has no info", output);
                continue;
            };
            let Some(name) = info.name.clone() else {
                tracing::warn!("Output {:?} has no name", output);
                continue;
            };
            let Some(logical_position) = info.logical_position else {
                tracing::warn!("Output {:?} has no position", output);
                continue;
            };
            let Some(logical_size) = info.logical_size else {
                tracing::warn!("Output {:?} has no size", output);
                continue;
            };
            outputs.push(Output {
                output,
                logical_position,
                logical_size,
                name,
            });
        }
        if outputs.is_empty() {
            tracing::error!("No output");
            return PortalResponse::Other;
        };

        // if interactive, send image to be used by screenshot editor & await response via channel
        if options.interactive.unwrap_or_default() {
            let (tx, mut rx) = tokio::sync::mpsc::channel(1);
            let first_output = &*outputs[0].name;
            let output_images = self
                .interactive_output_images(&outputs, app_id)
                .await
                .unwrap_or_default();
            let toplevel_images = self
                .interactive_toplevel_images(&outputs)
                .await
                .unwrap_or_default();
            // Keep the untouched pixels: Translate repaints these, so a second
            // translation starts from the original glyphs, not the first result.
            let output_images_pristine = output_images
                .iter()
                .map(|(name, image)| (name.clone(), image.rgba.clone()))
                .collect();
            // TODO: Maybe replace config's Choice with Choice from this file
            let choice = match config.choice {
                config::screenshot::Choice::Output(Some(output))
                    if outputs.iter().any(|Output { name, .. }| output == *name) =>
                {
                    Choice::Output(output)
                }
                config::screenshot::Choice::Output(_) => Choice::Output(first_output.into()),
                config::screenshot::Choice::Rectangle => {
                    // Use saved rectangle from config if available
                    let rect = config
                        .last_rectangle
                        .map(|r| Rect {
                            left: r.left,
                            top: r.top,
                            right: r.right,
                            bottom: r.bottom,
                        })
                        .unwrap_or_default();
                    Choice::Rectangle(rect, DragState::default())
                }
                config::screenshot::Choice::Window => Choice::Window(first_output.into(), None),
            };
            if let Err(err) = self
                .tx
                .send(subscription::Event::Screenshot(Args {
                    handle: handle.to_owned(),
                    app_id: app_id.to_string(),
                    parent_window: parent_window.to_string(),
                    action: if options.choose_destination.unwrap_or_default() {
                        Action::SaveToClipboard
                    } else {
                        Action::ReturnPath
                    },
                    options,
                    output_images,
                    toplevel_images,
                    tx,
                    location: config.save_location,
                    // TODO cover all outputs at start of rectangle?
                    choice,
                    // will be updated
                    pristine: output_images_pristine,
                    target_index: target_index(),
                    translating: false,
                    status: None,
                }))
                .await
            {
                tracing::error!("Failed to send screenshot event, {}", err);
                return PortalResponse::Other;
            }
            if let Some(res) = rx.recv().await {
                return res;
            } else {
                return PortalResponse::Cancelled::<ScreenshotResult>;
            }
        }

        let doc_path = match self.screenshot_inner(&outputs, app_id).await {
            Ok(res) => res,
            Err(err) => {
                tracing::error!("Failed to capture screenshot: {}", err);
                return PortalResponse::Other;
            }
        };

        // connection.object_server().remove::<Request, _>(&handle);
        PortalResponse::Success(ScreenshotResult {
            uri: format!("file:///{}", doc_path.display()),
        })
    }

    async fn pick_color(
        &self,
        handle: zvariant::ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        option: HashMap<String, zvariant::Value<'_>>,
    ) -> PortalResponse<PickColorResult> {
        // TODO create handle
        // XXX implement
        PortalResponse::Other
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        2
    }
}

pub(crate) fn view(portal: &CosmicPortal, id: window::Id) -> cosmic::Element<'_, Msg> {
    let Some((i, output)) = portal.outputs.iter().enumerate().find(|(i, o)| o.id == id) else {
        return space::horizontal().width(Length::Fixed(1.0)).into();
    };
    let Some(args) = portal.screenshot_args.as_ref() else {
        return space::horizontal().width(Length::Fixed(1.0)).into();
    };

    let Some(img) = args.output_images.get(&output.name) else {
        return space::horizontal().width(Length::Fixed(1.0)).into();
    };
    let theme = portal.core.system_theme().cosmic();
    // Window picker mode has no persistent selected area of its own, so there
    // is nothing for Translate to repaint there.
    let translate_enabled = match &args.choice {
        Choice::Rectangle(rect, _) => rect.dimensions().is_some(),
        Choice::Output(_) => true,
        Choice::Window(..) => false,
    };
    KeyboardWrapper::new(
        crate::widget::screenshot::ScreenshotSelection::new(
            args.choice.clone(),
            img,
            Msg::Capture,
            Msg::Cancel,
            output,
            id,
            Msg::OutputChanged,
            Msg::Choice,
            &args.toplevel_images,
            Msg::WindowChosen,
            &portal.location_options,
            args.location as usize,
            Msg::Location,
            TranslateControls {
                languages: lang::LANGS,
                selected: args.target_index,
                busy: args.translating,
                enabled: translate_enabled,
                status: args.status.as_deref(),
                on_translate: Msg::Translate {
                    output: output.name.clone(),
                },
                on_language: Box::new(Msg::TargetLanguage),
            },
            theme.spacing,
            i as u128,
        ),
        |key, modifiers| {
            if modifiers.control() {
                match key {
                    Key::Named(Named::Copy) => {
                        return Some(Msg::CaptureWithLocation(ImageSaveLocation::Clipboard));
                    }
                    Key::Named(Named::Save) => {
                        return Some(Msg::CaptureWithLocation(ImageSaveLocation::Pictures));
                    }
                    Key::Character(ref value) => {
                        let value = value.as_str();
                        if value.eq_ignore_ascii_case("c") {
                            return Some(Msg::CaptureWithLocation(ImageSaveLocation::Clipboard));
                        } else if value.eq_ignore_ascii_case("s") {
                            return Some(Msg::CaptureWithLocation(ImageSaveLocation::Pictures));
                        }
                    }
                    _ => {}
                }
            }

            match key {
                Key::Named(Named::Enter) => Some(Msg::Capture),
                Key::Named(Named::Escape) => Some(Msg::Cancel),
                _ => None,
            }
        },
    )
    .into()
}

pub fn update_msg(
    portal: &mut CosmicPortal,
    msg: Msg,
) -> cosmic::Task<cosmic::Action<crate::app::Msg>> {
    match msg {
        Msg::Capture => {
            let mut cmds: Vec<cosmic::Task<cosmic::Action<crate::app::Msg>>> = portal
                .outputs
                .iter()
                .map(|o| destroy_layer_surface(o.id))
                .collect();
            let Some(args) = portal.screenshot_args.take() else {
                tracing::error!("Failed to find screenshot Args for Capture message.");
                return cosmic::Task::batch(cmds);
            };
            let outputs = portal.outputs.clone();
            let Args {
                tx,
                choice,
                output_images: mut images,
                location,
                ..
            } = args;

            let mut success = true;
            let image_path = Screenshot::get_img_path(location);

            match choice {
                Choice::Output(name) => {
                    if let Some(img) = images.remove(&name) {
                        if let Ok(buffer) = Screenshot::save_rgba(&img.rgba, image_path.as_deref())
                            .inspect_err(|err| {
                                tracing::error!("Failed to capture screenshot: {:?}", err);
                                success = false;
                            })
                        {
                            cmds.push(clipboard::write_data(ScreenshotBytes::new(buffer)));
                        }
                    } else {
                        tracing::error!("Failed to find output {}", name);
                        success = false;
                    }
                }
                Choice::Rectangle(r, s) => {
                    if let Some(RectDimension { width, height }) = r.dimensions() {
                        // Construct Rgba image with size of rect
                        // then overlay the part of each image that intersects with the rect
                        //let mut img = RgbaImage::new(width.get(), height.get());

                        let frames = images
                            .into_iter()
                            .filter_map(|(name, raw_img)| {
                                let output = outputs.iter().find(|o| o.name == name)?;
                                let pos = output.logical_pos;
                                let output_rect = Rect {
                                    left: pos.0,
                                    top: pos.1,
                                    right: pos.0 + output.logical_size.0 as i32,
                                    bottom: pos.1 + output.logical_size.1 as i32,
                                };

                                let intersect = r.intersect(output_rect)?;

                                Some((raw_img.rgba, output_rect))
                            })
                            .collect::<Vec<_>>();
                        let img = combined_image(r, frames);

                        if let Ok(buffer) = Screenshot::save_rgba(&img, image_path.as_deref())
                            .inspect_err(|err| {
                                tracing::error!("Failed to capture screenshot: {:?}", err);
                                success = false;
                            })
                        {
                            cmds.push(clipboard::write_data(ScreenshotBytes::new(buffer)));
                        }
                    } else {
                        success = false;
                    }
                }
                Choice::Window(output, Some(window_i)) => {
                    if let Some(img) = args
                        .toplevel_images
                        .get(&output)
                        .and_then(|imgs| imgs.get(window_i))
                    {
                        if let Ok(buffer) = Screenshot::save_rgba(&img.rgba, image_path.as_deref())
                            .inspect_err(|err| {
                                tracing::error!("Failed to capture screenshot: {:?}", err);
                                success = false;
                            })
                        {
                            cmds.push(clipboard::write_data(ScreenshotBytes::new(buffer)));
                        }
                    } else {
                        success = false;
                    }
                }
                _ => {
                    success = false;
                }
            }

            let response = if success && let Some(image_path) = image_path {
                PortalResponse::Success(ScreenshotResult {
                    uri: format!("file:///{}", image_path.display()),
                })
            } else if success && image_path.is_none() {
                PortalResponse::Success(ScreenshotResult {
                    uri: "clipboard:///".to_string(),
                })
            } else {
                PortalResponse::Other
            };

            tokio::spawn(async move {
                if let Err(err) = tx.send(response).await {
                    tracing::error!("Failed to send screenshot event");
                }
            });
            cosmic::Task::batch(cmds)
        }
        Msg::CaptureWithLocation(location) => {
            if let Some(args) = portal.screenshot_args.as_mut() {
                args.location = location;
            } else {
                tracing::error!("Failed to find screenshot Args for CaptureWithLocation message.");
                return cosmic::Task::none();
            }
            update_msg(portal, Msg::Capture)
        }
        Msg::Cancel => {
            let cmds = portal.outputs.iter().map(|o| destroy_layer_surface(o.id));
            let Some(args) = portal.screenshot_args.take() else {
                tracing::error!("Failed to find screenshot Args for Cancel message.");
                return cosmic::Task::batch(cmds);
            };
            let Args { tx, .. } = args;
            tokio::spawn(async move {
                if let Err(err) = tx.send(PortalResponse::Cancelled).await {
                    tracing::error!("Failed to send screenshot event");
                }
            });

            cosmic::Task::batch(cmds)
        }
        Msg::Choice(c) => {
            let choice = (&c).into();
            // Only save config when drag is finished to avoid disk writes on every mouse motion
            let should_save_config =
                !matches!(&c, Choice::Rectangle(_, s) if *s != DragState::None);
            let last_rect = if let Choice::Rectangle(r, _) = &c {
                portal.prev_rectangle = Some(*r);
                Some(config::screenshot::Rect {
                    left: r.left,
                    top: r.top,
                    right: r.right,
                    bottom: r.bottom,
                })
            } else {
                portal.config.screenshot.last_rectangle
            };

            if let Some(args) = portal.screenshot_args.as_mut() {
                args.choice = c;
            } else {
                tracing::error!("Failed to find screenshot Args for Choice message.");
            }
            if should_save_config {
                cosmic::task::message(crate::app::Msg::ConfigSetScreenshot(
                    config::screenshot::Screenshot {
                        choice,
                        last_rectangle: last_rect,
                        ..portal.config.screenshot
                    },
                ))
            } else {
                cosmic::Task::none()
            }
        }
        Msg::OutputChanged(wl_output) => {
            if let (Some(args), Some(o)) = (
                portal.screenshot_args.as_mut(),
                portal
                    .outputs
                    .iter()
                    .find(|o| o.output == wl_output)
                    .map(|o| o.name.clone()),
            ) {
                args.choice = Choice::Output(o);
            } else {
                tracing::error!(
                    "Failed to find output for OutputChange message: {:?}",
                    wl_output
                );
            }
            portal.active_output = Some(wl_output);
            cosmic::Task::none()
        }
        Msg::WindowChosen(name, i) => {
            if let Some(args) = portal.screenshot_args.as_mut() {
                args.choice = Choice::Window(name, Some(i));
            } else {
                tracing::error!("Failed to find screenshot Args for WindowChosen message.");
            }
            update_msg(portal, Msg::Capture)
        }
        Msg::Location(loc) => {
            if let Some(args) = portal.screenshot_args.as_mut() {
                let loc = match loc {
                    loc if loc == ImageSaveLocation::Clipboard as usize => {
                        ImageSaveLocation::Clipboard
                    }
                    loc if loc == ImageSaveLocation::Pictures as usize => {
                        ImageSaveLocation::Pictures
                    }
                    loc if loc == ImageSaveLocation::Documents as usize => {
                        ImageSaveLocation::Documents
                    }
                    _ => args.location,
                };
                args.location = loc;
                cosmic::task::message(crate::app::Msg::ConfigSetScreenshot(
                    config::screenshot::Screenshot {
                        save_location: loc,
                        choice: (&mut portal.config.screenshot.choice).into(),
                        last_rectangle: portal.config.screenshot.last_rectangle,
                    },
                ))
            } else {
                tracing::error!("Failed to find screenshot Args for Location message.");
                cosmic::Task::none()
            }
        }
        Msg::TargetLanguage(index) => {
            if let Some(args) = portal.screenshot_args.as_mut() {
                if index < lang::LANGS.len() {
                    args.target_index = index;
                }
            } else {
                tracing::error!("Failed to find screenshot Args for TargetLanguage message.");
            }
            cosmic::Task::none()
        }
        Msg::Translate { output } => {
            if portal.screenshot_args.is_none() {
                tracing::error!("Failed to find screenshot Args for Translate message.");
                return cosmic::Task::none();
            }
            if portal
                .screenshot_args
                .as_ref()
                .is_some_and(|args| args.translating)
            {
                return cosmic::Task::none();
            }

            // Resolve the same area Capture would export: the chosen output
            // whole, or the drawn rectangle clipped to the output this dialog
            // belongs to. `output` is the dialog's own output; a rectangle is
            // drawn in that output's view.
            let choice = portal
                .screenshot_args
                .as_ref()
                .map(|args| args.choice.clone());
            let (name, selection) = match choice {
                Some(Choice::Output(name)) => (name, None),
                Some(Choice::Rectangle(rect, _)) => (output, Some(rect)),
                Some(Choice::Window(..)) => {
                    set_status(
                        portal,
                        Some(
                            "Translate works in area or screen mode; pick an area first"
                                .to_string(),
                        ),
                    );
                    return cosmic::Task::none();
                }
                _ => return cosmic::Task::none(),
            };

            let Some(base) = portal
                .screenshot_args
                .as_ref()
                .and_then(|args| args.pristine.get(&name).cloned())
            else {
                set_status(
                    portal,
                    Some("the capture for this screen is no longer available".to_string()),
                );
                return cosmic::Task::none();
            };

            let Some(output_state) = portal.outputs.iter().find(|state| state.name == name) else {
                set_status(
                    portal,
                    Some("this screen is no longer connected".to_string()),
                );
                return cosmic::Task::none();
            };
            let output_rect = LogicalRect {
                left: output_state.logical_pos.0,
                top: output_state.logical_pos.1,
                right: output_state.logical_pos.0 + output_state.logical_size.0 as i32,
                bottom: output_state.logical_pos.1 + output_state.logical_size.1 as i32,
            };
            let dimensions = base.dimensions();
            let region = match selection {
                Some(rect) => translate::region_for_selection(
                    LogicalRect {
                        left: rect.left,
                        top: rect.top,
                        right: rect.right,
                        bottom: rect.bottom,
                    },
                    output_rect,
                    dimensions,
                ),
                None => Some(Region::whole(dimensions)),
            };
            let Some(region) = region else {
                set_status(portal, Some("select an area to translate".to_string()));
                return cosmic::Task::none();
            };

            let Some(target) = portal.screenshot_args.as_ref().map(|args| args.target()) else {
                return cosmic::Task::none();
            };
            if let Some(args) = portal.screenshot_args.as_mut() {
                args.translating = true;
                args.status = None;
            }

            // OCR and the HTTP call take seconds; they run off the UI thread so
            // the dialog stays responsive and a slow sidecar never freezes it.
            cosmic::Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        translate::run(name, base, region, target)
                    })
                    .await
                    .unwrap_or_else(|error| {
                        Err(format!("the translation task failed: {error}"))
                    })
                },
                |result| cosmic::Action::App(crate::app::Msg::Screenshot(Msg::Translated(result))),
            )
        }
        Msg::Translated(result) => {
            let Some(args) = portal.screenshot_args.as_mut() else {
                tracing::error!("Failed to find screenshot Args for Translated message.");
                return cosmic::Task::none();
            };
            args.translating = false;
            match result {
                Ok(done) => {
                    let Some(base) = args.pristine.get(&done.output) else {
                        args.status =
                            Some("the capture for this screen is no longer available".to_string());
                        return cosmic::Task::none();
                    };
                    // Composite first, then swap the buffer: the dialog never
                    // shows or exports a half-painted image.
                    let composited = translate::composite(base, &done);
                    let (width, height) = composited.dimensions();
                    let handle = cosmic::widget::image::Handle::from_rgba(
                        width,
                        height,
                        composited.as_raw().clone(),
                    );
                    args.output_images.insert(
                        done.output,
                        ScreenshotImage {
                            rgba: composited,
                            handle,
                        },
                    );
                    args.status = Some(if done.drawn == done.lines {
                        format!(
                            "{} → {} · {} line(s)",
                            done.source, done.target.name, done.drawn
                        )
                    } else {
                        format!(
                            "{} → {} · {} of {} line(s) drawn",
                            done.source, done.target.name, done.drawn, done.lines
                        )
                    });
                }
                Err(message) => args.status = Some(message),
            }
            cosmic::Task::none()
        }
    }
}

/// Set the one-line feedback under the toolbar, if the dialog still exists.
fn set_status(portal: &mut CosmicPortal, status: Option<String>) {
    if let Some(args) = portal.screenshot_args.as_mut() {
        args.status = status;
    }
}

pub fn update_args(
    portal: &mut CosmicPortal,
    args: Args,
) -> cosmic::Task<cosmic::Action<crate::app::Msg>> {
    let Args {
        handle,
        app_id,
        parent_window,
        options,
        output_images: images,
        tx,
        choice,
        action,
        location,
        toplevel_images,
        ..
    } = &args;

    if portal.outputs.len() != images.len() {
        tracing::error!(
            "Screenshot output count mismatch: {} != {}",
            portal.outputs.len(),
            images.len()
        );
        tracing::warn!("Screenshot outputs: {:?}", portal.outputs);
        tracing::warn!("Screenshot images: {:?}", images.keys().collect::<Vec<_>>());
        return cosmic::Task::none();
    }

    // update output bg sources
    if let Ok(c) = cosmic::cosmic_config::Config::new_state(
        cosmic_bg_config::NAME,
        cosmic_bg_config::state::State::version(),
    ) {
        let bg_state = match cosmic_bg_config::state::State::get_entry(&c) {
            Ok(state) => state,
            Err((err, s)) => {
                tracing::error!("Failed to get bg config state: {:?}", err);
                s
            }
        };
        for o in &mut portal.outputs {
            let source = bg_state.wallpapers.iter().find(|s| s.0 == o.name);
            o.bg_source = Some(source.cloned().map(|s| s.1).unwrap_or_else(|| {
                cosmic_bg_config::Source::Path(
                    "/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg".into(),
                )
            }));
        }
    } else {
        tracing::error!("Failed to get bg config state");
        for o in &mut portal.outputs {
            o.bg_source = Some(cosmic_bg_config::Source::Path(
                "/usr/share/backgrounds/cosmic/orion_nebula_nasa_heic0601a.jpg".into(),
            ));
        }
    }
    portal.location_options = vec![
        fl!("save-to", "clipboard"),
        fl!("save-to", "pictures"),
        fl!("save-to", "documents"),
    ];

    if portal.screenshot_args.replace(args).is_none() {
        // iterate over outputs and create a layer surface for each
        let cmds: Vec<_> = portal
            .outputs
            .iter()
            .map(
                |OutputState {
                     output, id, name, ..
                 }| {
                    let id = *id;
                    let output = output.clone();
                    let name = name.clone();
                    cosmic::surface::surface_task::<crate::app::Msg>(
                cosmic::surface::action::simple_layer_shell::<crate::app::Msg>(
                    Default::default,
                        move || {
                            SctkLayerSurfaceSettings {
                                    id,
                                    layer: Layer::Overlay,
                                    keyboard_interactivity: KeyboardInteractivity::Exclusive,
                                    input_zone: None,
                                    anchor: Anchor::all(),
                                    output: IcedOutput::Output(output.clone()),
                                    namespace: "screenshot".to_string(),
                                    size: Some((None, None)),
                                    exclusive_zone: -1,
                                    size_limits: Limits::NONE.min_height(1.0).min_width(1.0),
                                    ..Default::default()
                                }
                            },
                        None::<fn() -> cosmic::Element<'static, cosmic::Action<crate::app::Msg>>>,
                        ),
                    )
                },
            )
            .collect();
        cosmic::Task::batch(cmds)
    } else {
        tracing::info!("Existing screenshot args updated");
        cosmic::Task::none()
    }
}
