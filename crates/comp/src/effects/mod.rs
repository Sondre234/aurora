//! Visual effects: shader programs and the elements built on them. Everything that needs
//! the GPU is compiled once at startup (`init`, called right after a renderer is created), so
//! no frame ever pays for compilation. Streams add their programs to `Programs` and one
//! `compile` line each; the element code lives in sibling modules (`corners`, `shadow`,
//! `blur`).

use smithay::backend::renderer::gles::{GlesError, GlesPixelProgram, GlesRenderer, GlesTexProgram};

pub mod corners;
pub mod shadow;

pub mod blur;

/// Every compiled program. Cheap to clone (programs are reference counted), so a frame copies
/// it out of the renderer and keeps using the renderer mutably.
#[derive(Clone, Default)]
pub struct Programs {
    /// Rounded corner clip for window surfaces.
    pub corners: Option<GlesTexProgram>,
    pub shadow: Option<GlesPixelProgram>,
    pub border: Option<GlesPixelProgram>,
    /// How many programs `compile` built, for the startup log line.
    compiled: usize,
    /// Dual-Kawase down and up passes; `None` when compilation failed.
    pub blur: Option<blur::BlurPrograms>,
}

impl Programs {
    /// Compiles every program, logging and skipping any that fails so a driver quirk costs
    /// one effect and not the session.
    fn compile(renderer: &mut GlesRenderer) -> Self {
        let mut programs = Self::default();
        programs.blur = blur::BlurPrograms::compile(renderer);
        programs.corners = compile("corners", || {
            renderer.compile_custom_texture_shader(corners::SHADER, &corners::uniform_names())
        });
        programs.shadow = compile("shadow", || {
            renderer
                .compile_custom_pixel_shader(shadow::SHADOW_SHADER, &shadow::shadow_uniform_names())
        });
        programs.border = compile("border", || {
            renderer
                .compile_custom_pixel_shader(shadow::BORDER_SHADER, &shadow::border_uniform_names())
        });
        programs.compiled = 2 * usize::from(programs.blur.is_some())
            + usize::from(programs.corners.is_some())
            + usize::from(programs.shadow.is_some())
            + usize::from(programs.border.is_some());
        programs
    }

    pub fn compiled(&self) -> usize {
        self.compiled
    }
}

fn compile<P>(name: &str, build: impl FnOnce() -> Result<P, GlesError>) -> Option<P> {
    match build() {
        Ok(program) => Some(program),
        Err(err) => {
            tracing::warn!("effects: shader {name} failed to compile: {err}");
            None
        }
    }
}

/// Compiles all programs for `renderer` and stores them on its context. Idempotent.
pub fn init(renderer: &mut GlesRenderer) {
    if programs(renderer).is_some() {
        return;
    }
    let programs = Programs::compile(renderer);
    tracing::info!("effects: programs compiled={}", programs.compiled());
    renderer
        .egl_context()
        .user_data()
        .insert_if_missing(|| programs);
}

/// The programs of `renderer`, `None` before `init`.
pub fn programs(renderer: &GlesRenderer) -> Option<Programs> {
    renderer
        .egl_context()
        .user_data()
        .get::<Programs>()
        .cloned()
}
