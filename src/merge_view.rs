//! The merge editor: a conflicted file's conflicts one under another, each with ours, the
//! base and theirs side by side. A side's button takes it whole; pressing a line keeps or
//! drops it. Unchanged lines show only around the conflicts.
use rune::{Cursor, El, TextStyle, Tx, col, row, text};
use rune_kit::{Button, ButtonVariant, ControlSize, Dialog, Kit};

use crate::Collate;
use crate::merge::{Chunk, From, Merge, Take};

/// Unchanged lines shown on each side of a conflict.
const CONTEXT: usize = 3;
/// Lines a change merged from one side shows before it's cut short.
const SHOWN: usize = 12;
const LINE_H: f32 = 18.0;

fn mono(k: &Kit) -> TextStyle {
    TextStyle { size: 12.0, line_height: LINE_H, ..k.ty.mono }
}

/// A line as drawn: without its line break, tabs as spaces.
fn shown(l: &str) -> String {
    l.trim_end_matches(['\n', '\r']).replace('\t', "    ")
}

fn line(k: &Kit, key: String, l: &str, color: rune::Color, fill: Option<rune::Color>) -> El {
    let mut r = row().key(key).w_full().h(LINE_H).px(8.0).items_center().child(text(shown(l)).font(mono(k)).color(color).truncate().min_w(0.0).grow(1.0));
    if let Some(f) = fill {
        r = r.fill(f);
    }
    r
}

fn note(k: &Kit, key: String, say: String) -> El {
    row().key(key).h(LINE_H).px(8.0).items_center().child(text(say).font(k.ty.text_small).color(k.c.text_faint))
}

/// Moves to the next or previous conflict and scrolls it into view.
pub fn step(s: &mut Collate, forward: bool, tx: &mut Tx) {
    if let Some(m) = &mut s.merge {
        m.step(forward);
        tx.scroll_into_view(format!("mc{}", m.current));
    }
}

/// Settles the conflict being read with a whole side, then moves on to the next unsettled one.
pub fn take(s: &mut Collate, take: Take, tx: &mut Tx) {
    if let Some(m) = &mut s.merge {
        m.take(m.current, take);
        if m.unsettled() > 0 {
            step(s, true, tx);
        }
    }
}

pub fn merge_dialog(m: &Merge, k: &Kit, layer: &mut Vec<El>) {
    let conflicts = m.conflicts();
    let mono = mono(k);
    let mut body = col().key("merge-chunks").w_full().gap(4.0);
    let mut n = 0;
    for (c, chunk) in m.chunks.iter().enumerate() {
        match chunk {
            Chunk::Clean { lines, from: From::Both } => {
                // Only the lines next to a conflict, or to a change merged in.
                let before = c > 0;
                let after = c + 1 < m.chunks.len();
                let head = if before { lines.len().min(CONTEXT) } else { 0 };
                let tail = if after { lines.len().saturating_sub(head).min(CONTEXT) } else { 0 };
                for (i, l) in lines[..head].iter().enumerate() {
                    body = body.child(line(k, format!("u{c}-{i}"), l, k.c.text_muted, None));
                }
                let hidden = lines.len() - head - tail;
                if hidden > 0 {
                    body = body.child(note(k, format!("u{c}-hidden"), format!("{hidden} unchanged line{}", if hidden == 1 { "" } else { "s" })));
                }
                for (i, l) in lines[lines.len() - tail..].iter().enumerate() {
                    body = body.child(line(k, format!("u{c}-t{i}"), l, k.c.text_muted, None));
                }
            }
            Chunk::Clean { lines, from } => {
                let side = if *from == From::Ours { "ours" } else { "theirs" };
                let mut block = col().key(format!("auto{c}")).w_full().radius(k.radius.sm).fill(k.alpha(k.c.success.icon, 0.08));
                block = block.child(note(k, format!("auto{c}-what"), format!("Merged from {side}")));
                for (i, l) in lines.iter().take(SHOWN).enumerate() {
                    block = block.child(line(k, format!("auto{c}-{i}"), l, k.c.text_base, None));
                }
                if lines.len() > SHOWN {
                    block = block.child(note(k, format!("auto{c}-more"), format!("{} more lines", lines.len() - SHOWN)));
                }
                body = body.child(block);
            }
            Chunk::Conflict { .. } => {
                body = body.child(conflict(k, m, n, chunk));
                n += 1;
            }
        }
    }
    let unsettled = m.unsettled();
    let auto = m.auto_merged();
    let mut summary = match conflicts.len() {
        0 => "No conflicts".to_owned(),
        1 => "1 conflict".to_owned(),
        c => format!("{c} conflicts"),
    };
    if unsettled > 0 {
        summary.push_str(&format!(" - {unsettled} to settle"));
    }
    if auto > 0 {
        summary.push_str(&format!(" - {auto} change{} merged by themselves", if auto == 1 { "" } else { "s" }));
    }
    let nav = |key: &str, label: &str, help: &str, forward: bool| {
        Button::new(label.to_owned()).variant(ButtonVariant::Ghost).control_size(ControlSize::Small).help(help.to_owned()).disabled(conflicts.len() < 2)
            .on_press_tx(move |s: &mut Collate, tx: &mut Tx| step(s, forward, tx)).build(k).key(key.to_owned())
    };
    let top = row().key("merge-top").w_full().px(16.0).pb(8.0).gap(4.0).items_center()
        .child(text(summary).font(k.ty.text_small).color(k.c.text_muted).grow(1.0).key("merge-summary"))
        .child(nav("merge-prev", "Previous", "Previous conflict (p)", false))
        .child(nav("merge-next", "Next", "Next conflict (n)", true));
    let scroll = col().key("merge-scroll").w_full().h(520.0).px(16.0).scroll(rune::ScrollSpec { stick_bottom: false }).child(body.font(mono));
    let save_help = if unsettled > 0 { "Settle every conflict first" } else if m.output.is_some() { "Write the merged file for git" } else { "Write the merged file and mark it resolved" };
    let buttons = row().key("merge-buttons").w_full().px(16.0).py(12.0).gap(8.0).justify_end()
        .child(Button::new("Cancel").variant(ButtonVariant::Ghost).on_press(|s: &mut Collate| s.merge = None).build(k).key("merge-cancel"))
        .child(Button::new("Save").variant(ButtonVariant::Contrast).disabled(unsettled > 0).help(save_help).on_press(|s: &mut Collate| s.save_merge()).build(k).key("merge-save"));
    Dialog::new("merge", format!("Merge - {}", m.path))
        .open(true)
        .width(1200.0)
        .fit()
        .click_outside(false)
        .on_close(|s: &mut Collate| s.merge = None)
        .child(top)
        .child(scroll)
        .child(buttons)
        .build(k, layer);
}

/// Conflict `n`: its actions, then ours, the base and theirs side by side, then what it
/// becomes.
fn conflict(k: &Kit, m: &Merge, n: usize, chunk: &Chunk) -> El {
    let Chunk::Conflict { base, ours, theirs, pick } = chunk else { return col() };
    let pick = pick.as_ref();
    let current = m.current == n;
    let act = |key: &str, label: &str, help: &str, how: Take| {
        Button::new(label.to_owned()).variant(ButtonVariant::Ghost).control_size(ControlSize::Small).help(help.to_owned())
            .on_press_tx(move |s: &mut Collate, tx: &mut Tx| {
                if let Some(m) = &mut s.merge {
                    m.current = n;
                }
                take(s, how, tx);
            })
            .build(k)
            .key(format!("{key}-{n}"))
    };
    let head = row().key(format!("mc{n}-head")).w_full().px(8.0).py(4.0).gap(2.0).items_center()
        .child(text(format!("Conflict {}", n + 1)).font(k.ty.text_xsmall_medium).color(if pick.is_some() { k.c.text_muted } else { k.c.danger.fg }).grow(1.0))
        .child(act("take-ours", "Ours", "Keep this branch's lines (1)", Take::Ours))
        .child(act("take-theirs", "Theirs", "Keep the incoming lines (2)", Take::Theirs))
        .child(act("take-both", "Both", "Ours, then theirs (3)", Take::Both))
        .child(act("take-both-rev", "Theirs Then Ours", "Theirs, then ours", Take::BothTheirsFirst))
        .child(act("take-base", "Neither", "Keep the lines as they were before either change", Take::Base));
    let side = |title: &str, key: &str, lines: &[String], keep: Option<&Vec<bool>>, theirs_side: Option<bool>, hue: rune::Color| {
        let mut c = col().key(format!("{key}{n}")).w(0.0).grow(1.0).min_w(0.0)
            .child(row().h(LINE_H).px(8.0).items_center().child(text(title.to_owned()).font(k.ty.small_medium).color(k.c.text_faint)));
        if lines.is_empty() {
            c = c.child(note(k, format!("{key}{n}-none"), "Nothing".to_owned()));
        }
        for (i, l) in lines.iter().enumerate() {
            let kept = keep.is_some_and(|k| k.get(i).copied().unwrap_or(false));
            let fill = if kept { k.alpha(hue, 0.16) } else { k.alpha(hue, 0.05) };
            let mut r = line(k, format!("{key}{n}-{i}"), l, if theirs_side.is_none() || kept || keep.is_none() { k.c.text_base } else { k.c.text_faint }, Some(fill));
            if let Some(t) = theirs_side {
                r = r.cursor(Cursor::Pointer).role("checkbox").label(format!("Keep line {} of {}", i + 1, if t { "theirs" } else { "ours" })).on_press(move |s: &mut Collate| {
                    if let Some(m) = &mut s.merge {
                        m.current = n;
                        m.toggle(n, t, i);
                    }
                });
            }
            c = c.child(r);
        }
        c
    };
    let sides = row().key(format!("mc{n}-sides")).w_full().gap(8.0).items_start()
        .child(side("Ours", "mo", ours, pick.map(|p| &p.ours), Some(false), k.c.info.icon))
        .child(side("Base", "mb", base, None, None, k.c.text_faint))
        .child(side("Theirs", "mt", theirs, pick.map(|p| &p.theirs), Some(true), k.c.warning.icon));
    let mut result = col().key(format!("mc{n}-result")).w_full().pt(4.0)
        .child(row().h(LINE_H).px(8.0).items_center().child(text("Result").font(k.ty.small_medium).color(k.c.text_faint)));
    match chunk.result() {
        None => result = result.child(note(k, format!("mc{n}-open"), "Take a side, or press the lines to keep".to_owned())),
        Some(lines) if lines.is_empty() => result = result.child(note(k, format!("mc{n}-empty"), "No lines".to_owned())),
        Some(lines) => {
            for (i, l) in lines.iter().enumerate() {
                result = result.child(line(k, format!("mr{n}-{i}"), l, k.c.text_base, Some(k.alpha(k.c.success.icon, 0.1))));
            }
        }
    }
    col().key(format!("mc{n}")).w_full().my(6.0).pb(8.0).radius(k.radius.md)
        .border(1.0, if current { k.c.info.icon } else { k.c.border_weak })
        .child(head)
        .child(col().w_full().px(8.0).child(sides).child(result))
}
