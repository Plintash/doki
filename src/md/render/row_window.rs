//! Window repeated rows inside a top-level list/table without changing text
//! geometry. A content commit discards this ledger: only unchanged pulse
//! frames reuse it. Giant individual rows and text blocks keep the full path.

use super::*;

#[derive(Clone, Default)]
struct Row {
    height: Option<Pixels>,
    ordinals: Range<usize>,
    block_break: bool,
    end_block_break: bool,
}

pub(super) struct Rows {
    rows: Vec<Row>,
    /// Includes the final row's trailing gap, which `plan` subtracts.
    offsets: Option<Vec<Pixels>>,
    /// Calculating table fractions scans every cell; do it once per revision.
    widths: Vec<f32>,
    #[cfg(test)]
    pub(super) built: usize,
    #[cfg(test)]
    pub(super) measured: usize,
    #[cfg(test)]
    offset_rebuilds: usize,
}

impl Rows {
    fn new(count: usize, widths: Vec<f32>) -> Self {
        Self {
            rows: vec![Row::default(); count],
            offsets: None,
            widths,
            #[cfg(test)]
            built: 0,
            #[cfg(test)]
            measured: 0,
            #[cfg(test)]
            offset_rebuilds: 0,
        }
    }

    pub(super) fn measure(&mut self, index: usize, height: Pixels) {
        #[cfg(test)]
        {
            self.measured += 1;
        }
        if self.rows[index].height != Some(height) {
            self.rows[index].height = Some(height);
            self.offsets = None;
        }
    }

    fn plan(&mut self, window: MessageBodyWindow, gap: Pixels) -> Option<Plan> {
        if self.rows.is_empty() {
            return None;
        }
        if self.offsets.is_none() {
            let mut offsets = Vec::with_capacity(self.rows.len() + 1);
            let mut next = Pixels::ZERO;
            offsets.push(next);
            for row in &self.rows {
                next += row.height? + gap;
                offsets.push(next);
            }
            self.offsets = Some(offsets);
            #[cfg(test)]
            {
                self.offset_rebuilds += 1;
            }
        }
        let offsets = self.offsets.as_ref().unwrap();
        let count = self.rows.len();
        let total = offsets[count] - gap;
        let from = px(window.visible_top - MARKDOWN_WINDOW_MARGIN);
        let to = px(window.visible_top + window.visible_height + MARKDOWN_WINDOW_MARGIN);
        let start = offsets[1..].partition_point(|end| *end - gap <= from);
        let end = offsets[..count].partition_point(|start| *start < to);
        if start >= end {
            return Some(Plan {
                range: 0..0,
                before: Some(total),
                after: None,
            });
        }
        Some(Plan {
            range: start..end,
            // Keep even zero-height spacers: the flex gap is still owed.
            before: (start > 0).then(|| offsets[start] - gap),
            after: (end < count).then(|| total - offsets[end]),
        })
    }
}

struct Plan {
    range: Range<usize>,
    before: Option<Pixels>,
    after: Option<Pixels>,
}

/// Called only for top-level blocks in a streaming pass that records heights.
/// Ordinary nested lists/tables retain their existing renderer and ordinals.
pub(super) fn render(
    block: &Block,
    ctx: &Ctx,
    window: Option<MessageBodyWindow>,
) -> Option<AnyElement> {
    let view = ctx.cache?;
    let ordinal = ctx.next_ordinal.get();
    match block {
        Block::List {
            ordered_start,
            items,
        } => {
            let rows = view
                .row_windows
                .borrow_mut()
                .entry(ordinal)
                .or_insert_with(|| Rc::new(RefCell::new(Rows::new(items.len(), Vec::new()))))
                .clone();
            ctx.starts_block.set(true);
            Some(list_column(
                render_rows(
                    &rows,
                    window,
                    px(ctx.metrics.block_gap * 0.5),
                    ctx,
                    |index| render_list_item(*ordered_start, index, &items[index], ctx),
                ),
                ctx,
            ))
        }
        Block::Table {
            header,
            rows: body,
            align,
        } => {
            let header_count = usize::from(!header.is_empty());
            let rows = view
                .row_windows
                .borrow_mut()
                .entry(ordinal)
                .or_insert_with(|| {
                    let columns = header
                        .len()
                        .max(body.iter().map(Vec::len).max().unwrap_or(0));
                    Rc::new(RefCell::new(Rows::new(
                        body.len() + header_count,
                        if columns == 0 {
                            Vec::new()
                        } else {
                            column_widths(header, body, columns)
                        },
                    )))
                })
                .clone();
            let widths = rows.borrow().widths.clone();
            if widths.is_empty() {
                return Some(div().into_any_element());
            }
            ctx.starts_block.set(true);
            // The row ledger excludes the table's two outer border pixels.
            let window = window.map(|window| MessageBodyWindow {
                visible_top: window.visible_top - 1.0,
                ..window
            });
            Some(
                table_column(ctx)
                    .children(render_rows(&rows, window, Pixels::ZERO, ctx, |index| {
                        if header_count == 1 && index == 0 {
                            table_row(header, &widths, align, ctx, FontWeight::SEMIBOLD, true)
                                .bg(ctx.palette.overlay)
                                .into_any_element()
                        } else {
                            let index = index - header_count;
                            table_row(
                                &body[index],
                                &widths,
                                align,
                                ctx,
                                FontWeight::NORMAL,
                                index + 1 < body.len(),
                            )
                            .into_any_element()
                        }
                    }))
                    .into_any_element(),
            )
        }
        _ => None,
    }
}

fn render_rows(
    rows: &Rc<RefCell<Rows>>,
    window: Option<MessageBodyWindow>,
    gap: Pixels,
    ctx: &Ctx,
    mut render: impl FnMut(usize) -> AnyElement,
) -> Vec<AnyElement> {
    let plan = window.and_then(|window| rows.borrow_mut().plan(window, gap));
    let range = plan
        .as_ref()
        .map_or_else(|| 0..rows.borrow().rows.len(), |plan| plan.range.clone());
    let mut children = Vec::with_capacity(range.len() + 2);
    if let Some(height) = plan.as_ref().and_then(|plan| plan.before) {
        children.push(spacer_element(height));
    }
    for index in range {
        #[cfg(test)]
        {
            rows.borrow_mut().built += 1;
        }
        if plan.is_some() {
            let rows = rows.borrow();
            ctx.next_ordinal.set(rows.rows[index].ordinals.start);
            ctx.starts_block.set(rows.rows[index].block_break);
        }
        let start = ctx.next_ordinal.get();
        let block_break = ctx.starts_block.get();
        let inner = render(index);
        if plan.is_none() {
            let mut rows = rows.borrow_mut();
            rows.rows[index].ordinals = start..ctx.next_ordinal.get();
            rows.rows[index].block_break = block_break;
            rows.rows[index].end_block_break = ctx.starts_block.get();
        }
        children.push(
            Measured {
                inner,
                sink: MeasureSink::Row {
                    rows: rows.clone(),
                    index,
                },
            }
            .into_any_element(),
        );
    }
    if let Some(plan) = plan {
        if let Some(height) = plan.after {
            children.push(spacer_element(height));
        }
        // Skipped rows still occupy their canonical text identities. In
        // particular, skipping a table header must not renumber its cells.
        if let Some(last) = rows.borrow().rows.last() {
            ctx.next_ordinal.set(last.ordinals.end);
            ctx.starts_block.set(last.end_block_break);
        }
    }
    children
}

#[cfg(test)]
mod tests;
