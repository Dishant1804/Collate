---
name: ui-change
description: Use when changing how Collate looks - layout, spacing, type, colours, controls, or any view function in src/main.rs, src/history_view.rs, src/settings_view.rs, src/comments.rs, src/map_view.rs or src/diff/view.rs.
---

# Changing Collate's UI

## Before editing

1. Find the view function and read it whole. Views are plain functions returning `El`.
2. Find how the same kind of thing is drawn elsewhere, and reuse it. Look in the kit
   first (`rune-kit` in the cargo git checkout): `Button`, `IconButton`, `TextInput`,
   `SidebarRow`, `SidebarHeader`, `Tree`, `Dialog`, `Select`, `Switch`.

## Tokens

- Type: `k.ty.ui` and `ui_medium` (13), `text_small` (13), `text_xsmall` and
  `text_xsmall_medium` (12), `small` and `small_medium` (11), `mono`. Code and counts of
  added and removed lines are mono; everything else is sans.
- Colour: `k.c.text_base`, `text_muted`, `text_faint`, `border_weak`, `bg_base`,
  `success`, `danger`, `warning`, `info`. No literal colours.
- Radius: `k.radius.sm` (4) for small controls and pills, `k.radius.md` (6) for buttons
  and boxes.
- Spacing: 4, 8, 12, 16. Insets: `EDGE` (16, the diff and its summary), `PANEL_INSET`
  (12, the staging panel), `COLUMN_TOP` (6, every column's top).
- Heights: 20 for small controls, 24 for icon buttons, 28 for list rows and section heads.
- A ghost button pads its label by `GHOST_PAD`; pull it out by that much (`.mr(-GHOST_PAD)`)
  so the label, not the invisible box, sits on the edge.

## Rules

- A section title is `text_xsmall_medium` in `text_muted`, its count in `text_faint`.
- A text action is a small ghost `Button`. Don't build clickable text by hand.
- Separators in text are ` - `, never `·`.
- Every interactive element has a `role`, a `label`, and `help` where the action isn't
  obvious from its text.
- Give elements a `key`. Tests find elements by key, and rows keep state by it.

## After editing

Run `cargo test`. The alignment tests (`the_three_columns_start_on_one_line`,
`the_diff_has_one_edge_each_side`, `the_staging_panel_lines_up`) fail when a
column moves. Then look at the app (`cargo run -- <repo>`), in split and unified, light
and dark. Say plainly if you could not look at it.
