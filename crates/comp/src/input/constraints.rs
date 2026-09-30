//! pointer-constraints: locks and confinements. Motion consults the constraint of the
//! surface with pointer focus; one only holds while that surface also has the keyboard.
use smithay::{
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
    wayland::{
        compositor::RegionAttributes,
        pointer_constraints::{
            ConstraintRemove, PointerConstraint, PointerConstraintsHandler, with_pointer_constraint,
        },
        seat::WaylandFocus,
    },
};

use crate::{Aurora, focus::FocusTarget};

pub struct Constraint {
    pub surface: WlSurface,
    pub locked: bool,
    pub active: bool,
    pub region: Option<RegionAttributes>,
}

fn surface_of(focus: Option<FocusTarget>) -> Option<WlSurface> {
    focus?.wl_surface().map(|s| s.into_owned())
}

impl Aurora {
    /// Whether `surface` has both pointer and keyboard focus, the condition to hold a constraint.
    fn constraint_may_hold(&self, surface: &WlSurface) -> bool {
        surface_of(self.pointer.current_focus()).as_ref() == Some(surface)
            && surface_of(self.keyboard.current_focus()).as_ref() == Some(surface)
    }

    /// The constraint of the surface with pointer focus. One that is active but has lost the
    /// keyboard is released here, so a background window never keeps the pointer.
    pub fn current_constraint(&self) -> Option<Constraint> {
        let surface = surface_of(self.pointer.current_focus())?;
        let holds = self.constraint_may_hold(&surface);
        with_pointer_constraint(&surface, &self.pointer, |c| {
            let c = c?;
            if c.is_active() && !holds {
                c.deactivate();
                return None;
            }
            Some(Constraint {
                locked: matches!(*c, PointerConstraint::Locked(_)),
                active: c.is_active(),
                region: c.region().cloned(),
                surface: surface.clone(),
            })
        })
    }

    /// Where `surface` sits, if it is what is under `pos`.
    fn surface_origin(
        &self,
        surface: &WlSurface,
        pos: Point<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        let (target, origin) = self.surface_under(pos)?;
        (target.wl_surface().as_deref() == Some(surface)).then_some(origin)
    }

    /// Moves a confined pointer to the nearest allowed position: the target, else with one
    /// axis kept, else where it was.
    pub fn confine(
        &self,
        c: &Constraint,
        old: Point<f64, Logical>,
        new: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        let inside = |p: Point<f64, Logical>| {
            self.surface_origin(&c.surface, p).is_some_and(|origin| {
                c.region
                    .as_ref()
                    .is_none_or(|r| r.contains((p - origin).to_i32_floor()))
            })
        };
        [new, (new.x, old.y).into(), (old.x, new.y).into()]
            .into_iter()
            .find(|p| inside(*p))
            .unwrap_or(old)
    }

    /// After the pointer moved to `pos`: an inactive constraint whose region it entered starts
    /// holding.
    pub fn activate_constraint_at(&mut self, pos: Point<f64, Logical>) {
        let Some(c) = self.current_constraint() else {
            return;
        };
        if c.active {
            return;
        }
        let Some(origin) = self.surface_origin(&c.surface, pos) else {
            return;
        };
        if c.region
            .as_ref()
            .is_none_or(|r| r.contains((pos - origin).to_i32_floor()))
        {
            self.activate_constraint(&c.surface);
        }
    }

    fn activate_constraint(&self, surface: &WlSurface) {
        with_pointer_constraint(surface, &self.pointer, |c| {
            if let Some(c) = c {
                c.activate();
            }
        });
    }
}

impl PointerConstraintsHandler for Aurora {
    fn new_constraint(
        &mut self,
        surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        if self.constraint_may_hold(surface) {
            self.activate_constraint(surface);
        }
    }

    /// A destroyed lock hands the cursor back at the position the client asked for.
    fn remove_constraint(
        &mut self,
        surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
        remove: ConstraintRemove,
    ) {
        let ConstraintRemove::Destroyed(constraint) = remove else {
            return;
        };
        let PointerConstraint::Locked(lock) = &constraint else {
            return;
        };
        let Some(hint) = lock
            .cursor_position_hint()
            .filter(|_| constraint.is_active())
        else {
            return;
        };
        if let Some(origin) = self.surface_origin(surface, self.pointer.current_location()) {
            self.warp_pointer(origin + hint);
        }
    }
}
