//! Visual effects: shader programs and the elements built on them. Everything that needs
//! the GPU is compiled once at startup (`init`, called right after a renderer is created), so
//! no frame ever pays for compilation. Streams add their programs to `Programs` and one
//! `compile` line each; the element code lives in sibling modules (`corners`, `shadow`,
//! `blur`).
#![allow(dead_code)] // filled in by the M3 step 2 streams

use smithay::backend::renderer::gles::GlesRenderer;

/// Every compiled program. Cheap to clone (programs are reference counted), so a frame copies
/// it out of the renderer and keeps using the renderer mutably.
#[derive(Clone, Default)]
pub struct Programs {
    /// How many programs `compile` built, for the startup log line.
    compiled: usize,
}

impl Programs {
    /// Compiles every program, logging and skipping any that fails so a driver quirk costs
    /// one effect and not the session.
    fn compile(_renderer: &mut GlesRenderer) -> Self {
        let programs = Self::default();
        // Streams: compile here, e.g.
        //   programs.corners = compile("corners", || renderer.compile_custom_texture_shader(..));
        //   programs.compiled += 1;
        programs
    }

    pub fn compiled(&self) -> usize {
        self.compiled
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
