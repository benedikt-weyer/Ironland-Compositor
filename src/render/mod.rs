//! Rendering pipeline, drawing helpers, overlays and wallpaper handling.

pub mod border;
#[cfg(any(feature = "udev", feature = "xwayland"))]
pub mod cursor;
pub mod drawing;
pub mod font;
pub mod key_tracker;
pub mod perf_overlay;
pub mod rounded_corners;
pub mod tint;
pub mod wallpaper;

use smithay::{
    backend::renderer::{
        Color32F, ImportAll, ImportMem, Renderer,
        damage::{Error as OutputDamageTrackerError, OutputDamageTracker, RenderOutputResult},
        element::{
            AsRenderElements, Element, Id, RenderElement, Wrap,
            memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            surface::WaylandSurfaceRenderElement,
            utils::{
                ConstrainAlign, ConstrainScaleBehavior, CropRenderElement, RelocateRenderElement,
                RescaleRenderElement,
            },
        },
    },
    desktop::{
        LayerSurface, layer_map_for_output,
        space::{
            ConstrainBehavior, ConstrainReference, Space, SpaceElement, SpaceRenderElements,
            constrain_space_element,
        },
    },
    output::Output,
    utils::{Logical, Point, Rectangle, Scale, Size},
    wayland::{
        compositor::{TraversalAction, with_surface_tree_downward},
        shell::wlr_layer::Layer as WlrLayer,
    },
};

#[cfg(feature = "debug")]
use crate::drawing::FpsElement;
use crate::{
    drawing::{CLEAR_COLOR, CLEAR_COLOR_FULLSCREEN, PointerRenderElement},
    rounded_corners::GlesCapable,
    shell::{FullscreenSurface, WindowElement, WindowRenderElement, tiling},
};

/// Background of the screen lock (see `crate::lock`).
const LOCK_CLEAR_COLOR: Color32F = Color32F::new(0.08, 0.07, 0.07, 1.0);

/// Border thickness (logical pixels) of the tiling drag-and-drop indicator.
const DROP_INDICATOR_THICKNESS: u32 = 3;
/// Border color of the tiling drag-and-drop indicator (`#rrggbbaa`).
const DROP_INDICATOR_COLOR: &str = "#89b4fadd";

smithay::backend::renderer::element::render_elements! {
    pub CustomRenderElements<R> where
        R: ImportAll + ImportMem + GlesCapable;
    Pointer=PointerRenderElement<R>,
    Surface=WaylandSurfaceRenderElement<R>,
    // Shared by every compositor-drawn, software-rasterized screen overlay
    // (the launcher and the workspace-switcher dots): they're all just a
    // `MemoryRenderBuffer` placed at some location, so one variant covers them.
    Overlay=MemoryRenderBufferRenderElement<R>,
    Blur=CropRenderElement<MemoryRenderBufferRenderElement<R>>,
    // A live, scaled-down window drawn inside a workspace overview slot (see
    // `crate::overview::thumbnail_elements`).
    Thumbnail=CropRenderElement<CropRenderElement<RelocateRenderElement<RescaleRenderElement<WindowRenderElement<R>>>>>,
    // The focus-highlight border around the currently focused window (see
    // `crate::border`).
    Border=crate::border::BorderRenderElement,
    // The night-mode tint over the whole output (see `crate::tint`).
    Tint=smithay::backend::renderer::element::solid::SolidColorRenderElement,
    #[cfg(feature = "debug")]
    // Note: We would like to borrow this element instead, but that would introduce
    // a feature-dependent lifetime, which introduces a lot more feature bounds
    // as the whole type changes and we can't have an unused lifetime (for when "debug" is disabled)
    // in the declaration.
    Fps=FpsElement<R::TextureId>,
}

impl<R: Renderer> std::fmt::Debug for CustomRenderElements<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pointer(arg0) => f.debug_tuple("Pointer").field(arg0).finish(),
            Self::Surface(arg0) => f.debug_tuple("Surface").field(arg0).finish(),
            Self::Overlay(arg0) => f.debug_tuple("Overlay").field(arg0).finish(),
            Self::Blur(arg0) => f.debug_tuple("Blur").field(arg0).finish(),
            Self::Thumbnail(arg0) => f.debug_tuple("Thumbnail").field(arg0).finish(),
            Self::Border(_) => f.debug_tuple("Border").finish(),
            Self::Tint(arg0) => f.debug_tuple("Tint").field(arg0).finish(),
            #[cfg(feature = "debug")]
            Self::Fps(arg0) => f.debug_tuple("Fps").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

smithay::backend::renderer::element::render_elements! {
    pub OutputRenderElements<R, E> where R: ImportAll + ImportMem + GlesCapable;
    Space=SpaceRenderElements<R, E>,
    Window=Wrap<E>,
    Custom=CustomRenderElements<R>,
    Preview=CropRenderElement<RelocateRenderElement<RescaleRenderElement<WindowRenderElement<R>>>>,
    // A tiled window's own surfaces, cut off at its tile (see `clip_to_tile`).
    Clipped=CropRenderElement<WindowRenderElement<R>>,
}

impl<R: Renderer + ImportAll + ImportMem, E: RenderElement<R> + std::fmt::Debug> std::fmt::Debug
    for OutputRenderElements<R, E>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Space(arg0) => f.debug_tuple("Space").field(arg0).finish(),
            Self::Window(arg0) => f.debug_tuple("Window").field(arg0).finish(),
            Self::Custom(arg0) => f.debug_tuple("Custom").field(arg0).finish(),
            Self::Preview(arg0) => f.debug_tuple("Preview").field(arg0).finish(),
            Self::Clipped(arg0) => f.debug_tuple("Clipped").field(arg0).finish(),
            Self::_GenericCatcher(arg0) => f.debug_tuple("_GenericCatcher").field(arg0).finish(),
        }
    }
}

pub fn space_preview_elements<'a, R, C>(
    renderer: &'a mut R,
    space: &'a Space<WindowElement>,
    output: &'a Output,
) -> impl Iterator<Item = C> + 'a
where
    R: Renderer + ImportAll + ImportMem + GlesCapable,
    R::TextureId: Clone + Send + 'static,
    C: From<CropRenderElement<RelocateRenderElement<RescaleRenderElement<WindowRenderElement<R>>>>>
        + 'a,
{
    let constrain_behavior = ConstrainBehavior {
        reference: ConstrainReference::BoundingBox,
        behavior: ConstrainScaleBehavior::Fit,
        align: ConstrainAlign::CENTER,
    };

    let preview_padding = 10;

    let elements_on_space = space.elements_for_output(output).count();
    let output_scale = output.current_scale().fractional_scale();
    let output_transform = output.current_transform();
    let output_size = output
        .current_mode()
        .map(|mode| {
            output_transform
                .transform_size(mode.size)
                .to_f64()
                .to_logical(output_scale)
        })
        .unwrap_or_default();

    let max_elements_per_row = 4;
    let elements_per_row = usize::min(elements_on_space, max_elements_per_row);
    let rows = f64::ceil(elements_on_space as f64 / elements_per_row as f64);

    let preview_size = Size::from((
        f64::round(output_size.w / elements_per_row as f64) as i32 - preview_padding * 2,
        f64::round(output_size.h / rows) as i32 - preview_padding * 2,
    ));

    space
        .elements_for_output(output)
        .enumerate()
        .flat_map(move |(element_index, window)| {
            let column = element_index % elements_per_row;
            let row = element_index / elements_per_row;
            let preview_location = Point::from((
                preview_padding + (preview_padding + preview_size.w) * column as i32,
                preview_padding + (preview_padding + preview_size.h) * row as i32,
            ));
            let constrain = Rectangle::new(preview_location, preview_size);
            constrain_space_element(
                renderer,
                window,
                preview_location,
                1.0,
                output_scale,
                constrain,
                constrain_behavior,
            )
        })
}

#[profiling::function]
#[allow(clippy::too_many_arguments)]
pub fn output_elements<R>(
    output: &Output,
    space: &Space<WindowElement>,
    custom_elements: impl IntoIterator<Item = CustomRenderElements<R>>,
    background_element: Option<CustomRenderElements<R>>,
    blurred_background: Option<&MemoryRenderBuffer>,
    renderer: &mut R,
    show_window_preview: bool,
    focused_window_rect: Option<Rectangle<i32, Logical>>,
    border: &crate::config::BorderSettings,
    corner_radius: f32,
    border_cache: &mut crate::border::BorderCache,
    drop_indicator: Option<Rectangle<i32, Logical>>,
    drop_indicator_cache: &mut crate::border::BorderCache,
    locked: bool,
    overview: bool,
) -> (
    Vec<OutputRenderElements<R, WindowRenderElement<R>>>,
    Color32F,
)
where
    R: Renderer + ImportAll + ImportMem + GlesCapable,
    R::TextureId: Clone + Send + 'static,
{
    // Screen lock: draw only what the caller passed (the cursor and the
    // lock panel) - no windows, layer surfaces or wallpaper - over a flat
    // background, so nothing behind the lock is visible or capturable.
    if locked {
        let elements = custom_elements
            .into_iter()
            .map(OutputRenderElements::from)
            .collect::<Vec<_>>();
        return (elements, LOCK_CLEAR_COLOR);
    }

    // While the workspace overview is up the windows are drawn only as its
    // thumbnails: rendering the real ones too would put two elements with the
    // same surface ids in one frame, which confuses the damage tracker and
    // leaves some thumbnails blank.
    if let Some(window) = output
        .user_data()
        .get::<FullscreenSurface>()
        .and_then(|f| f.get())
        .filter(|_| !overview)
    {
        let scale = output.current_scale().fractional_scale().into();
        let window_render_elements: Vec<WindowRenderElement<R>> =
            AsRenderElements::<R>::render_elements(&window, renderer, (0, 0).into(), scale, 1.0);

        let elements = custom_elements
            .into_iter()
            .map(OutputRenderElements::from)
            .chain(
                window_render_elements
                    .into_iter()
                    .map(|e| OutputRenderElements::Window(Wrap::from(e))),
            )
            .collect::<Vec<_>>();
        (elements, CLEAR_COLOR_FULLSCREEN)
    } else {
        let mut output_render_elements = custom_elements
            .into_iter()
            .map(OutputRenderElements::from)
            .collect::<Vec<_>>();

        if show_window_preview && !overview && space.elements_for_output(output).count() > 0 {
            output_render_elements.extend(space_preview_elements(renderer, space, output));
        }

        let output_geometry = space.output_geometry(output).unwrap_or_default();
        let output_scale = output.current_scale().fractional_scale();

        // Background/bottom layer-shell surfaces (e.g. a shell's own
        // wallpaper) are rendered separately below, *behind* the blur, so
        // that an opaque client-drawn background can't hide it. Top/overlay
        // layers (bars, popups) still render above every window.
        let layer_map = layer_map_for_output(output);
        let (lower_layers, upper_layers): (Vec<&LayerSurface>, Vec<&LayerSurface>) =
            layer_map.layers().rev().partition(|surface| {
                matches!(surface.layer(), WlrLayer::Background | WlrLayer::Bottom)
            });

        let render_layers = |layers: &[&LayerSurface], renderer: &mut R| {
            layers
                .iter()
                .filter_map(|surface| {
                    layer_map
                        .layer_geometry(surface)
                        .map(|geo| (geo.loc, *surface))
                })
                .flat_map(|(loc, surface)| {
                    AsRenderElements::<R>::render_elements::<WaylandSurfaceRenderElement<R>>(
                        surface,
                        renderer,
                        loc.to_physical_precise_round(output_scale),
                        Scale::from(output_scale),
                        1.0,
                    )
                })
                .map(|element| OutputRenderElements::from(CustomRenderElements::Surface(element)))
                .collect::<Vec<_>>()
        };

        output_render_elements.extend(render_layers(&upper_layers, renderer));

        // The focus-highlight border, drawn in front of every window but
        // behind top/overlay layer-shell surfaces (bars, popups) pushed just
        // above, so a maximized/tiled window's border never paints over the
        // shell's own chrome (list order is front-to-back).
        if let Some(window_rect) = focused_window_rect.filter(|_| !overview) {
            // Follow the same cutoff the window itself gets, so the border
            // hugs the visible part instead of the window's full extent.
            let clip = space
                .elements_for_output(output)
                .find(|w| space.element_bbox(w) == Some(window_rect))
                .map(|w| window_clip(w, output_geometry, &layer_map));
            let mut window_rect = window_rect;
            window_rect.loc -= output_geometry.loc;
            let window_rect = match clip {
                Some(clip) => window_rect.intersection(clip).unwrap_or(window_rect),
                None => window_rect,
            };
            if let Some(element) = border_cache.build(renderer, window_rect, border, corner_radius) {
                output_render_elements.push(OutputRenderElements::from(CustomRenderElements::Border(element)));
            }
        }

        // The tiling drag-and-drop indicator, previewing where a dragged
        // tiled window would land if dropped now (see
        // `shell::tiling::drop_target`). Reuses the focus-border shader with
        // its own fixed style rather than the user's border config, since
        // it's a transient snap-preview, not the window-highlight feature.
        if let Some(mut indicator_rect) = drop_indicator {
            indicator_rect.loc -= output_geometry.loc;
            let indicator_style = crate::config::BorderSettings {
                enabled: true,
                thickness: DROP_INDICATOR_THICKNESS,
                color: DROP_INDICATOR_COLOR.to_string(),
                gradient_color: None,
                angle: 0.0,
            };
            if let Some(element) =
                drop_indicator_cache.build(renderer, indicator_rect, &indicator_style, 0.0)
            {
                output_render_elements.push(OutputRenderElements::from(CustomRenderElements::Border(element)));
            }
        }

        // Same result as `space.render_elements_for_region(renderer,
        // &output_geometry, output_scale, 1.0)` for every window with no
        // in-flight tiling animation (the overwhelming common case) - done
        // by hand, one window at a time, only so a window `tiling::
        // animated_rect` says is mid grow-in/reflow can be rendered
        // scaled/relocated into its current eased rect instead (see
        // `crate::shell::tiling`), which `Space`'s own helper has no hook
        // for.
        for window in space.elements_for_output(output).rev().filter(|_| !overview) {
            let Some(element_location) = space.element_location(window) else {
                continue;
            };
            let geometry_offset = SpaceElement::geometry(window).loc;

            if let Some((anim_rect, alpha)) = tiling::animated_rect(output, window) {
                let location = element_location - output_geometry.loc;
                let constrain = Rectangle::new(anim_rect.loc - output_geometry.loc, anim_rect.size);
                output_render_elements.extend(constrain_space_element(
                    renderer,
                    window,
                    location,
                    alpha,
                    output_scale,
                    constrain,
                    ConstrainBehavior {
                        reference: ConstrainReference::Geometry,
                        behavior: ConstrainScaleBehavior::Stretch,
                        align: ConstrainAlign::CENTER,
                    },
                ));
            } else {
                let render_location = (element_location - geometry_offset - output_geometry.loc)
                    .to_physical_precise_round(output_scale);
                let elements: Vec<WindowRenderElement<R>> = AsRenderElements::<R>::render_elements(
                    window,
                    renderer,
                    render_location,
                    Scale::from(output_scale),
                    1.0,
                );
                let tile = Some(window_clip(window, output_geometry, &layer_map))
                    .map(|rect| rect.to_f64().to_physical(output_scale).to_i32_round());
                let main_tree = tile.map(|_| surface_tree_ids(window));
                for element in elements {
                    let clipped = match (&tile, &main_tree) {
                        // Popups sit outside the window's surface tree and
                        // are meant to overhang the tile, so they stay uncut.
                        (Some(tile), Some(ids))
                            if ids.contains(element.id())
                                && !tile.contains_rect(element.geometry(Scale::from(output_scale))) =>
                        {
                            CropRenderElement::from_element(element, output_scale, *tile)
                                .map(OutputRenderElements::Clipped)
                        }
                        _ => Some(OutputRenderElements::Window(Wrap::from(element))),
                    };
                    output_render_elements.extend(clipped);
                }
            }
        }

        // Place a cropped blurred wallpaper immediately behind each window.
        // Opaque application pixels cover it completely; alpha-bearing pixels
        // naturally composite over it and produce the backdrop-blur effect.
        if let Some(blurred_background) = blurred_background {
            let output_bounds = Rectangle::from_size(output_geometry.size);
            for window in space.elements_for_output(output) {
                let Some(mut window_rect) = space.element_bbox(window) else {
                    continue;
                };
                window_rect.loc -= output_geometry.loc;
                let Some(window_rect) = window_rect.intersection(output_bounds) else {
                    continue;
                };
                let crop_rect = window_rect
                    .to_f64()
                    .to_physical(output_scale)
                    .to_i32_round();
                let Ok(element) = MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    Point::from((0.0, 0.0)),
                    blurred_background,
                    None,
                    None,
                    None,
                    smithay::backend::renderer::element::Kind::Unspecified,
                ) else {
                    continue;
                };
                if let Some(element) =
                    CropRenderElement::from_element(element, output_scale, crop_rect)
                {
                    output_render_elements.push(OutputRenderElements::from(
                        CustomRenderElements::Blur(element),
                    ));
                }
            }
        }

        output_render_elements.extend(render_layers(&lower_layers, renderer));
        drop(layer_map);

        // The wallpaper sits behind every window, so it's appended last:
        // damage-tracked render elements are painted front-to-back, in list
        // order.
        if let Some(background_element) = background_element {
            output_render_elements.push(OutputRenderElements::from(background_element));
        }

        (output_render_elements, CLEAR_COLOR)
    }
}

/// Where `window` may draw, in output-local logical coordinates: its tile if
/// it's tiled, otherwise the part of the output not reserved by layer
/// surfaces (panels etc.) - the same area tiles are carved from.
fn window_clip(
    window: &WindowElement,
    output_geometry: Rectangle<i32, Logical>,
    layer_map: &smithay::desktop::LayerMap,
) -> Rectangle<i32, Logical> {
    tiling::tile_clip(window)
        .map(|mut rect| {
            rect.loc -= output_geometry.loc;
            rect
        })
        .unwrap_or_else(|| layer_map.non_exclusive_zone())
}

/// Render-element ids of `window`'s root surface and all its subsurfaces -
/// everything except its popups.
fn surface_tree_ids(window: &WindowElement) -> Vec<Id> {
    let mut ids = Vec::new();
    if let Some(root) = window.wl_surface() {
        with_surface_tree_downward(
            &root,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, _, _| ids.push(Id::from_wayland_resource(surface)),
            |_, _, _| true,
        );
    }
    ids
}

#[allow(clippy::too_many_arguments)]
pub fn render_output<'a, 'd, R>(
    output: &'a Output,
    space: &'a Space<WindowElement>,
    custom_elements: impl IntoIterator<Item = CustomRenderElements<R>>,
    background_element: Option<CustomRenderElements<R>>,
    blurred_background: Option<&MemoryRenderBuffer>,
    renderer: &'a mut R,
    framebuffer: &'a mut R::Framebuffer<'_>,
    damage_tracker: &'d mut OutputDamageTracker,
    age: usize,
    show_window_preview: bool,
    focused_window_rect: Option<Rectangle<i32, Logical>>,
    border: &crate::config::BorderSettings,
    corner_radius: f32,
    border_cache: &mut crate::border::BorderCache,
    drop_indicator: Option<Rectangle<i32, Logical>>,
    drop_indicator_cache: &mut crate::border::BorderCache,
    locked: bool,
    overview: bool,
) -> Result<RenderOutputResult<'d>, OutputDamageTrackerError<R::Error>>
where
    R: Renderer + ImportAll + ImportMem + GlesCapable,
    R::TextureId: Clone + Send + 'static,
{
    let (elements, clear_color) = output_elements(
        output,
        space,
        custom_elements,
        background_element,
        blurred_background,
        renderer,
        show_window_preview,
        focused_window_rect,
        border,
        corner_radius,
        border_cache,
        drop_indicator,
        drop_indicator_cache,
        locked,
        overview,
    );
    damage_tracker.render_output(renderer, framebuffer, age, &elements, clear_color)
}
