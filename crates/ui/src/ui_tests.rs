//! Headless widget tests: layout, hit-testing, interaction state, editing, damage and
//! pixels. No Wayland, no display.

use crate::geom::{Color, Insets, Point, Rect, Size};
use crate::input::{Button, Id, Input, Key, KeyEvent, Mods, UiEvent};
use crate::skia::{PaintCaches, PixelBuffer};
use crate::text::{TextStyle, TextSystem};
use crate::ui::Ui;
use crate::widget::{Dim, Justify, List, ListItem, Node, TextInput};

const RED: Color = Color::rgb(200, 0, 0);
const BLUE: Color = Color::rgb(0, 0, 200);

fn boxed(w: f32, h: f32) -> Node {
    Node::spacer().size(Dim::Px(w), Dim::Px(h))
}

fn ui_of(root: Node, w: f32, h: f32) -> Ui {
    let mut ui = Ui::new(TextSystem::new(), root);
    ui.set_size(Size::new(w, h));
    ui.layout();
    ui
}

fn rect(ui: &Ui, id: u32) -> Rect {
    ui.rect_of(Id(id)).unwrap_or_else(|| panic!("no node {id}"))
}

fn mv(x: f32, y: f32) -> Input {
    Input::PointerMove(Point::new(x, y))
}

fn click(ui: &mut Ui, x: f32, y: f32) -> Vec<UiEvent> {
    let p = Point::new(x, y);
    let mut ev = ui.handle(Input::PointerMove(p));
    ev.extend(ui.handle(Input::PointerDown(p, Button::Left)));
    ev.extend(ui.handle(Input::PointerUp(p, Button::Left)));
    ev
}

fn key(ui: &mut Ui, k: Key) -> Vec<UiEvent> {
    ui.handle(Input::Key(KeyEvent::new(k)))
}

fn type_str(ui: &mut Ui, s: &str) {
    for c in s.chars() {
        ui.handle(Input::Key(KeyEvent::typed(c)));
    }
}

fn ctrl() -> Mods {
    Mods {
        ctrl: true,
        ..Mods::default()
    }
}

fn shift() -> Mods {
    Mods {
        shift: true,
        ..Mods::default()
    }
}

#[test]
fn row_fill_gap_padding() {
    let root = Node::row(vec![
        boxed(50.0, 20.0).id(1),
        Node::spacer().id(2).width(Dim::Fill(1.0)),
        boxed(30.0, 40.0).id(3),
    ])
    .padding(Insets::all(10.0))
    .gap(5.0);
    let ui = ui_of(root, 200.0, 100.0);
    assert_eq!(rect(&ui, 1), Rect::new(10.0, 40.0, 50.0, 20.0)); // centered in 80px of height
    assert_eq!(rect(&ui, 2).x, 65.0);
    assert_eq!(rect(&ui, 2).w, 200.0 - 20.0 - 50.0 - 30.0 - 10.0);
    assert_eq!(rect(&ui, 3).x, 190.0 - 30.0);
    assert_eq!(rect(&ui, 3).y, 30.0);
}

#[test]
fn fill_weights_split_remaining() {
    let root = Node::row(vec![
        boxed(40.0, 10.0).id(1),
        boxed(0.0, 10.0).id(2).width(Dim::Fill(1.0)),
        boxed(0.0, 10.0).id(3).width(Dim::Fill(3.0)),
    ]);
    let ui = ui_of(root, 200.0, 10.0);
    assert_eq!(rect(&ui, 2).w, 40.0);
    assert_eq!(rect(&ui, 3).w, 120.0);
    assert_eq!(rect(&ui, 3).x, 80.0);
}

#[test]
fn column_justify_and_hidden_children() {
    let root = Node::column(vec![
        boxed(10.0, 10.0).id(1),
        boxed(10.0, 10.0).id(2).hidden(),
        boxed(10.0, 10.0).id(3),
    ])
    .gap(4.0)
    .justify(Justify::End);
    let mut ui = ui_of(root, 50.0, 100.0);
    assert_eq!(rect(&ui, 1).y, 100.0 - 24.0);
    assert_eq!(rect(&ui, 3).y, 100.0 - 10.0);
    ui.set_visible(Id(2), true);
    ui.layout();
    assert_eq!(rect(&ui, 3).y, 100.0 - 10.0);
    assert_eq!(rect(&ui, 1).y, 100.0 - 38.0);
}

#[test]
fn space_between_and_stack_alignment() {
    let root = Node::row(vec![
        boxed(10.0, 10.0).id(1),
        boxed(10.0, 10.0).id(2),
        boxed(10.0, 10.0).id(3),
    ])
    .justify(Justify::SpaceBetween);
    let ui = ui_of(root, 100.0, 10.0);
    assert_eq!(
        (rect(&ui, 1).x, rect(&ui, 2).x, rect(&ui, 3).x),
        (0.0, 45.0, 90.0)
    );
    let stack = Node::stack(vec![boxed(10.0, 10.0).id(1)])
        .justify(Justify::End)
        .align(crate::widget::Align::Center);
    let ui = ui_of(stack, 100.0, 50.0);
    assert_eq!(rect(&ui, 1), Rect::new(90.0, 20.0, 10.0, 10.0));
}

#[test]
fn labels_measure_and_wrap() {
    let t = TextSystem::new();
    if !t.has_fonts() {
        return;
    }
    let long = "a label with quite a few words that needs several lines to fit";
    let root = Node::column(vec![
        Node::label("short", TextStyle::default(), Color::WHITE).id(1),
        Node::label(long, TextStyle::default().wrapped(0), Color::WHITE).id(2),
        Node::label(long, TextStyle::default(), Color::WHITE).id(3),
    ]);
    let ui = ui_of(root, 120.0, 400.0);
    let (a, b, c) = (rect(&ui, 1), rect(&ui, 2), rect(&ui, 3));
    assert!(a.h > 10.0 && c.h > 10.0);
    assert!(
        b.h > a.h * 2.0,
        "wrapped label is taller: {} vs {}",
        b.h,
        a.h
    );
    assert!((c.h - a.h).abs() < 1.0, "ellipsized label stays one line");
    assert_eq!(b.y, a.bottom());
}

#[test]
fn hit_testing_prefers_topmost_and_respects_clip() {
    let root = Node::stack(vec![
        boxed(100.0, 100.0).id(1).clickable(),
        boxed(40.0, 40.0).id(2).clickable(),
        Node::stack(vec![boxed(200.0, 200.0).id(4).clickable()])
            .clip()
            .size(Dim::Px(30.0), Dim::Px(30.0))
            .id(3),
    ]);
    let mut ui = ui_of(root, 200.0, 200.0);
    // Children of the clipped stack are laid out at its origin; the third child overlays.
    assert_eq!(ui.hit_test(Point::new(10.0, 10.0)), Some(Id(4)));
    assert_eq!(ui.hit_test(Point::new(35.0, 35.0)), Some(Id(2)));
    assert_eq!(ui.hit_test(Point::new(90.0, 90.0)), Some(Id(1)));
    assert_eq!(ui.hit_test(Point::new(150.0, 150.0)), None);
    ui.set_visible(Id(2), false);
    ui.layout();
    assert_eq!(ui.hit_test(Point::new(35.0, 35.0)), Some(Id(1)));
}

fn button_ui() -> Ui {
    let root = Node::column(vec![
        Node::button("OK", TextStyle::default(), Color::WHITE)
            .id(1)
            .bg(RED)
            .hover_bg(BLUE)
            .press_bg(Color::BLACK)
            .width(Dim::Px(80.0)),
        boxed(10.0, 10.0).id(2),
    ])
    .align(crate::widget::Align::Start)
    .bg(Color::rgb(10, 10, 10));
    ui_of(root, 200.0, 100.0)
}

#[test]
fn click_hover_press_and_events() {
    let mut ui = button_ui();
    let b = rect(&ui, 1);
    assert_eq!(b.w, 80.0);
    let inside = Point::new(b.x + 5.0, b.y + 5.0);
    assert!(ui.handle(Input::PointerMove(inside)).is_empty());
    assert!(ui.node(Id(1)).is_some_and(|n| n.is_hovered()));
    ui.handle(Input::PointerDown(inside, Button::Left));
    assert!(ui.node(Id(1)).is_some_and(|n| n.is_pressed()));
    let ev = ui.handle(Input::PointerUp(inside, Button::Left));
    assert_eq!(ev, vec![UiEvent::Clicked(Id(1))]);
    // Release elsewhere does not click.
    ui.handle(Input::PointerDown(inside, Button::Left));
    let ev = ui.handle(Input::PointerUp(Point::new(150.0, 90.0), Button::Left));
    assert!(ev.is_empty());
    // Other buttons are ignored.
    assert!(
        ui.handle(Input::PointerDown(inside, Button::Right))
            .is_empty()
    );
    ui.handle(Input::PointerLeave);
    assert!(!ui.node(Id(1)).is_some_and(|n| n.is_hovered()));
}

#[test]
fn idle_ui_needs_no_redraw_and_damage_is_local() {
    let mut ui = button_ui();
    let mut caches = PaintCaches::new(ui.text().clone());
    let mut buf = PixelBuffer::new(200, 100);
    let first = ui.draw(&mut buf.painter(1.0, &mut caches));
    assert_eq!(first, vec![Rect::new(0.0, 0.0, 200.0, 100.0)]);
    assert!(!ui.needs_redraw());

    // Moving over empty space changes nothing.
    ui.handle(mv(190.0, 90.0));
    assert!(!ui.needs_redraw());

    // Hovering the button damages only the button.
    let b = rect(&ui, 1);
    ui.handle(mv(b.x + 2.0, b.y + 2.0));
    assert!(ui.needs_redraw());
    ui.layout();
    assert_eq!(ui.take_damage(), vec![b]);
    assert!(!ui.needs_redraw());
}

#[test]
fn partial_repaint_matches_full_repaint_even_when_translucent() {
    let root = || {
        Node::column(vec![
            Node::button("x", TextStyle::default(), Color::WHITE)
                .id(1)
                .bg(Color::rgba(255, 255, 255, 64))
                .hover_bg(Color::rgba(255, 0, 0, 128)),
            boxed(20.0, 20.0).id(2).bg(BLUE),
        ])
        .bg(Color::rgba(0, 0, 0, 100))
        .radius(8.0)
    };
    let mut a = ui_of(root(), 100.0, 80.0);
    let mut caches = PaintCaches::new(a.text().clone());
    let mut inc = PixelBuffer::new(100, 80);
    a.draw(&mut inc.painter(1.0, &mut caches));
    let b = rect(&a, 1);
    for p in [
        Point::new(b.x + 1.0, b.y + 1.0),
        Point::new(95.0, 75.0),
        Point::new(b.x + 2.0, b.y + 2.0),
    ] {
        a.handle(Input::PointerMove(p));
        let d = a.draw(&mut inc.painter(1.0, &mut caches));
        assert!(d.len() <= 1);
    }
    // A fresh UI in the same state, painted once in full.
    let mut f = ui_of(root(), 100.0, 80.0);
    f.handle(mv(b.x + 2.0, b.y + 2.0));
    let mut full = PixelBuffer::new(100, 80);
    f.draw(&mut full.painter(1.0, &mut caches));
    assert_eq!(
        inc.bytes(),
        full.bytes(),
        "incremental repaint must equal a full repaint"
    );
    // The rounded root corner stays transparent, the body keeps exactly its alpha.
    assert_eq!(inc.pixel(99, 79).a, 0);
    assert_eq!(inc.pixel(90, 60).a, 100);
}

#[test]
fn widget_pixels_and_fractional_scale() {
    let root =
        Node::row(vec![boxed(10.0, 10.0).bg(RED), boxed(10.0, 10.0).bg(BLUE)]).bg(Color::WHITE);
    let mut ui = ui_of(root, 20.0, 10.0);
    ui.set_scale(1.5);
    let mut caches = PaintCaches::new(ui.text().clone());
    let mut buf = PixelBuffer::new(30, 15);
    ui.draw(&mut buf.painter(1.5, &mut caches));
    assert_eq!(buf.pixel(7, 7), RED);
    assert_eq!(buf.pixel(22, 7), BLUE);
    assert_eq!(buf.pixel(14, 14), RED);
}

fn input_ui(initial: &str) -> Ui {
    let i = TextInput {
        text: initial.to_string(),
        cursor: initial.len(),
        ..TextInput::default()
    };
    let root = Node::column(vec![Node::input(i).id(1).width(Dim::Px(200.0))]);
    let mut ui = ui_of(root, 300.0, 100.0);
    ui.set_focus(Some(Id(1)));
    ui
}

#[test]
fn text_input_editing() {
    let mut ui = input_ui("");
    type_str(&mut ui, "héllo");
    assert_eq!(ui.text_of(Id(1)), Some("héllo"));
    assert_eq!(
        key(&mut ui, Key::Backspace),
        vec![UiEvent::TextChanged(Id(1))]
    );
    assert_eq!(ui.text_of(Id(1)), Some("héll"));
    key(&mut ui, Key::Left);
    key(&mut ui, Key::Left);
    type_str(&mut ui, "X");
    assert_eq!(ui.text_of(Id(1)), Some("héXll"));
    key(&mut ui, Key::Home);
    key(&mut ui, Key::Delete);
    assert_eq!(ui.text_of(Id(1)), Some("éXll"));
    // Multi-byte char moves by whole chars.
    key(&mut ui, Key::Right);
    key(&mut ui, Key::Backspace);
    assert_eq!(ui.text_of(Id(1)), Some("Xll"));
    assert_eq!(key(&mut ui, Key::Enter), vec![UiEvent::Submitted(Id(1))]);
    // Unhandled keys surface to the app.
    assert_eq!(
        key(&mut ui, Key::Escape),
        vec![UiEvent::Key(KeyEvent::new(Key::Escape))]
    );
    assert_eq!(
        key(&mut ui, Key::Down),
        vec![UiEvent::Key(KeyEvent::new(Key::Down))]
    );
}

#[test]
fn text_input_selection_words_and_ctrl_keys() {
    let mut ui = input_ui("one two three");
    ui.handle(Input::Key(KeyEvent::new(Key::Left).with_mods(ctrl())));
    ui.handle(Input::Key(KeyEvent::new(Key::Left).with_mods(ctrl())));
    type_str(&mut ui, "_");
    assert_eq!(ui.text_of(Id(1)), Some("one _two three"));
    ui.handle(Input::Key(KeyEvent::new(Key::Backspace).with_mods(ctrl())));
    assert_eq!(ui.text_of(Id(1)), Some("one two three"));
    // Shift+Left selects, typing replaces the selection.
    for _ in 0..4 {
        ui.handle(Input::Key(KeyEvent::new(Key::Right).with_mods(shift())));
    }
    type_str(&mut ui, "#");
    assert_eq!(ui.text_of(Id(1)), Some("one #three"));
    ui.handle(Input::Key(KeyEvent {
        key: Key::Char('a'),
        text: Some("\u{1}".into()),
        mods: ctrl(),
    }));
    type_str(&mut ui, "z");
    assert_eq!(ui.text_of(Id(1)), Some("z"));
    // Control chords that are not editing keys go to the app.
    let ev = ui.handle(Input::Key(KeyEvent {
        key: Key::Char('n'),
        text: Some("\u{e}".into()),
        mods: ctrl(),
    }));
    assert!(matches!(ev.as_slice(), [UiEvent::Key(_)]));
    ui.handle(Input::Key(KeyEvent {
        key: Key::Char('u'),
        text: None,
        mods: ctrl(),
    }));
    assert_eq!(ui.text_of(Id(1)), Some(""));
}

#[test]
fn text_input_click_places_caret_and_scrolls() {
    let t = TextSystem::new();
    if !t.has_fonts() {
        return;
    }
    let mut ui = input_ui("abcdefghij");
    let r = rect(&ui, 1);
    // Click at the far left of the text selects position 0; typing inserts there.
    click(&mut ui, r.x + 8.0, r.y + r.h / 2.0);
    type_str(&mut ui, "_");
    assert_eq!(ui.text_of(Id(1)), Some("_abcdefghij"));
    // Drag selection.
    let y = r.y + r.h / 2.0;
    ui.handle(Input::PointerDown(Point::new(r.x + 8.0, y), Button::Left));
    ui.handle(mv(r.x + 8.0 + 30.0, y));
    ui.handle(Input::PointerUp(Point::new(r.x + 38.0, y), Button::Left));
    type_str(&mut ui, "!");
    let s = ui.text_of(Id(1)).unwrap_or("");
    assert!(s.starts_with('!') && s.len() < 11 + 1, "{s}");
    // Long text keeps the caret visible by scrolling, rendering must not panic.
    type_str(&mut ui, &"w".repeat(80));
    let mut caches = PaintCaches::new(ui.text().clone());
    let mut buf = PixelBuffer::new(300, 100);
    ui.draw(&mut buf.painter(1.0, &mut caches));
    let lit = (0..300)
        .filter(|&x| buf.pixel(x, r.y as u32 + 10).a > 0)
        .count();
    assert!(lit > 0);
    // Nothing paints outside the input's clip.
    assert_eq!(buf.pixel(r.right() as u32 + 5, r.y as u32 + 10).a, 0);
}

#[test]
fn password_input_masks_display_but_not_text() {
    let i = TextInput {
        password: true,
        ..TextInput::default()
    };
    let root = Node::column(vec![Node::input(i).id(1)]);
    let mut ui = ui_of(root, 300.0, 50.0);
    ui.set_focus(Some(Id(1)));
    type_str(&mut ui, "pä5");
    assert_eq!(ui.text_of(Id(1)), Some("pä5"));
    key(&mut ui, Key::Left);
    key(&mut ui, Key::Backspace);
    assert_eq!(ui.text_of(Id(1)), Some("p5"));
}

fn list_ui(n: usize) -> Ui {
    let items = (0..n)
        .map(|i| ListItem::new(format!("item {i}")).tag(i as u64))
        .collect();
    let l = List {
        items,
        row_height: 20.0,
        ..List::default()
    };
    let root = Node::column(vec![
        Node::list(l).id(1).size(Dim::Px(100.0), Dim::Px(60.0)),
    ]);
    let mut ui = ui_of(root, 100.0, 100.0);
    ui.set_focus(Some(Id(1)));
    ui
}

#[test]
fn list_selection_scroll_and_events() {
    let mut ui = list_ui(10);
    assert_eq!(ui.list_selected(Id(1)), None);
    assert_eq!(
        key(&mut ui, Key::Down),
        vec![UiEvent::Selected {
            list: Id(1),
            index: 0
        }]
    );
    for _ in 0..4 {
        key(&mut ui, Key::Down);
    }
    assert_eq!(ui.list_selected(Id(1)), Some(4));
    let Some(crate::widget::Kind::List(l)) = ui.node(Id(1)).map(|n| n.kind.clone()) else {
        panic!()
    };
    assert_eq!(
        l.scroll(),
        4.0 * 20.0 + 20.0 - 60.0,
        "selection scrolled into view"
    );
    assert_eq!(
        key(&mut ui, Key::Enter),
        vec![UiEvent::Activated {
            list: Id(1),
            index: 4
        }]
    );
    key(&mut ui, Key::End);
    assert_eq!(ui.list_selected(Id(1)), Some(9));
    key(&mut ui, Key::Down); // clamped, no event
    assert!(key(&mut ui, Key::Down).is_empty());
    key(&mut ui, Key::Home);
    assert_eq!(ui.list_selected(Id(1)), Some(0));
    assert_eq!(ui.list_item(Id(1), 3).map(|i| i.tag), Some(3));
}

#[test]
fn list_click_hover_and_wheel() {
    let mut ui = list_ui(10);
    let r = rect(&ui, 1);
    let ev = click(&mut ui, r.x + 10.0, r.y + 30.0); // row 1
    assert_eq!(
        ev,
        vec![
            UiEvent::Selected {
                list: Id(1),
                index: 1
            },
            UiEvent::Activated {
                list: Id(1),
                index: 1
            }
        ]
    );
    ui.handle(Input::Scroll {
        pos: Point::new(r.x + 10.0, r.y + 10.0),
        dx: 0.0,
        dy: 40.0,
    });
    let ev = click(&mut ui, r.x + 10.0, r.y + 10.0); // row 2 after scrolling 40px
    assert_eq!(
        ev[0],
        UiEvent::Selected {
            list: Id(1),
            index: 2
        }
    );
    // Scroll is clamped to the content.
    ui.handle(Input::Scroll {
        pos: Point::new(r.x + 10.0, r.y + 10.0),
        dx: 0.0,
        dy: 10_000.0,
    });
    let ev = click(&mut ui, r.x + 10.0, r.y + 50.0);
    assert_eq!(
        ev[0],
        UiEvent::Selected {
            list: Id(1),
            index: 9
        }
    );
    // Replacing items resets the view and drops an out-of-range selection.
    ui.set_items(Id(1), vec![ListItem::new("only")]);
    assert_eq!(ui.list_selected(Id(1)), None);
}

#[test]
fn list_paints_visible_rows_only() {
    let t = TextSystem::new();
    if !t.has_fonts() {
        return;
    }
    let mut ui = list_ui(10_000);
    ui.list_select(Id(1), Some(2));
    let mut caches = PaintCaches::new(ui.text().clone());
    let mut buf = PixelBuffer::new(100, 100);
    ui.draw(&mut buf.painter(1.0, &mut caches));
    // Row 2 is selected: its background is the selection color; shaping only touched rows 0..3.
    assert_eq!(buf.pixel(95, 50), List::default().selected_bg);
    assert!(
        ui.text().stats().shaped.entries <= 8,
        "{:?}",
        ui.text().stats().shaped
    );
}

#[test]
fn tab_cycles_focus_and_clicks_focus_inputs() {
    let root = Node::column(vec![
        Node::input(TextInput::default()).id(1),
        Node::input(TextInput::default()).id(2),
    ]);
    let mut ui = ui_of(root, 200.0, 100.0);
    assert_eq!(ui.focus(), None);
    assert_eq!(
        key(&mut ui, Key::Tab),
        vec![UiEvent::FocusChanged(Some(Id(1)))]
    );
    assert_eq!(
        key(&mut ui, Key::Tab),
        vec![UiEvent::FocusChanged(Some(Id(2)))]
    );
    let ev = ui.handle(Input::Key(KeyEvent::new(Key::Tab).with_mods(shift())));
    assert_eq!(ev, vec![UiEvent::FocusChanged(Some(Id(1)))]);
    let r = rect(&ui, 2);
    let ev = click(&mut ui, r.x + 3.0, r.y + 3.0);
    assert_eq!(ev, vec![UiEvent::FocusChanged(Some(Id(2)))]);
    assert_eq!(ui.focus(), Some(Id(2)));
}

#[test]
fn edit_relayouts_and_damages_old_and_new_area() {
    let root = Node::row(vec![
        boxed(20.0, 10.0).id(1).bg(RED),
        boxed(20.0, 10.0).id(2).bg(BLUE),
    ]);
    let mut ui = ui_of(root, 100.0, 10.0);
    let _ = ui.take_damage();
    assert!(!ui.needs_redraw());
    ui.edit(Id(1), |n| n.style.width = Dim::Px(40.0));
    ui.layout();
    let d = ui.take_damage();
    assert_eq!(d, vec![Rect::new(0.0, 0.0, 60.0, 10.0)]);
    assert!(!ui.edit(Id(99), |_| {}));
}

#[test]
fn resize_and_scale_change_damage_everything() {
    let mut ui = ui_of(boxed(10.0, 10.0).id(1), 50.0, 50.0);
    let _ = ui.take_damage();
    ui.set_size(Size::new(60.0, 40.0));
    ui.layout();
    assert_eq!(ui.take_damage(), vec![Rect::new(0.0, 0.0, 60.0, 40.0)]);
    ui.set_scale(2.0);
    ui.layout();
    assert_eq!(ui.take_damage(), vec![Rect::new(0.0, 0.0, 60.0, 40.0)]);
    assert!(!ui.needs_redraw());
}

#[test]
fn canvas_paints_only_the_damaged_part_of_its_rect() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let seen: Rc<RefCell<Vec<(Rect, Rect)>>> = Rc::default();
    let log = seen.clone();
    let canvas = Node::canvas(move |p, node, region| {
        log.borrow_mut().push((node, region));
        p.fill_rect(region, RED);
    })
    .size(Dim::Fill(1.0), Dim::Fill(1.0));
    let mut ui = ui_of(canvas, 100.0, 50.0);
    let mut caches = PaintCaches::new(ui.text().clone());
    let mut buf = PixelBuffer::new(100, 50);
    ui.draw(&mut buf.painter(1.0, &mut caches));
    assert_eq!(seen.borrow().len(), 1);
    assert_eq!(seen.borrow()[0].1, Rect::new(0.0, 0.0, 100.0, 50.0));

    seen.borrow_mut().clear();
    ui.damage(Rect::new(10.0, 10.0, 20.0, 5.0));
    assert!(ui.needs_redraw());
    ui.draw(&mut buf.painter(1.0, &mut caches));
    let seen = seen.borrow();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, Rect::new(0.0, 0.0, 100.0, 50.0));
    assert_eq!(seen[0].1, Rect::new(10.0, 10.0, 20.0, 5.0));
}
