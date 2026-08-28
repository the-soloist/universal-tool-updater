use std::cmp;

use crate::display::{truncate, width as display_width};

#[derive(Debug)]
pub(super) struct MergedTable {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    widths: Vec<usize>,
}

impl MergedTable {
    pub(super) fn new(
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        available_width: usize,
    ) -> Self {
        let natural_widths = headers
            .iter()
            .enumerate()
            .map(|(column, header)| {
                rows.iter().fold(display_width(header), |width, row| {
                    cmp::max(width, display_width(&row[column]))
                })
            })
            .collect::<Vec<_>>();
        let widths = fit_widths(&natural_widths, available_width);
        Self {
            headers,
            rows,
            widths,
        }
    }

    pub(super) fn render(&self) -> String {
        let mut output = String::new();
        self.render_full_border(&mut output, '┌', '┬', '┐');
        self.render_row(&mut output, &self.headers, None);
        self.render_full_border(&mut output, '├', '┼', '┤');
        for (index, row) in self.rows.iter().enumerate() {
            let continuations = index
                .checked_sub(1)
                .map(|previous| merged_columns(&self.rows[previous], row));
            self.render_row(&mut output, row, continuations.as_deref());
            if index + 1 < self.rows.len() {
                let next = merged_columns(row, &self.rows[index + 1]);
                self.render_merged_border(&mut output, &next);
            }
        }
        self.render_full_border(&mut output, '└', '┴', '┘');
        output
    }

    fn render_row(&self, output: &mut String, cells: &[String], merged: Option<&[bool]>) {
        output.push('│');
        for (index, cell) in cells.iter().enumerate() {
            let hidden = merged.is_some_and(|columns| columns[index]);
            let value = truncate(if hidden { "" } else { cell }, self.widths[index]);
            output.push(' ');
            output.push_str(&value);
            output.extend(std::iter::repeat_n(
                ' ',
                self.widths[index] - display_width(&value) + 1,
            ));
            output.push('│');
        }
        output.push('\n');
    }

    fn render_full_border(&self, output: &mut String, left: char, middle: char, right: char) {
        output.push(left);
        for (index, width) in self.widths.iter().enumerate() {
            output.extend(std::iter::repeat_n('─', width + 2));
            output.push(if index + 1 == self.widths.len() {
                right
            } else {
                middle
            });
        }
        output.push('\n');
    }

    fn render_merged_border(&self, output: &mut String, continuations: &[bool]) {
        output.push(if continuations[0] { '│' } else { '├' });
        for (index, width) in self.widths.iter().enumerate() {
            let horizontal = !continuations[index];
            output.extend(std::iter::repeat_n(
                if horizontal { '─' } else { ' ' },
                width + 2,
            ));
            let junction = if index + 1 == self.widths.len() {
                if horizontal { '┤' } else { '│' }
            } else {
                match (horizontal, !continuations[index + 1]) {
                    (true, true) => '┼',
                    (true, false) => '┤',
                    (false, true) => '├',
                    (false, false) => '│',
                }
            };
            output.push(junction);
        }
        output.push('\n');
    }
}

pub(super) fn merged_columns(previous: &[String], current: &[String]) -> Vec<bool> {
    let last = current.len().saturating_sub(1);
    let mut same_parent = true;
    current
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            same_parent &= previous[index] == *cell;
            same_parent && !cell.is_empty() && index != last
        })
        .collect()
}

fn fit_widths(natural: &[usize], available_width: usize) -> Vec<usize> {
    let frame_width = natural.len() * 3 + 1;
    let content_budget = available_width
        .saturating_sub(frame_width)
        .max(natural.len());
    let mut widths = vec![1; natural.len()];
    let mut remaining = content_budget.saturating_sub(natural.len());
    let last = natural.len() - 1;
    let mut order = vec![last, 0];
    order.extend(1..last);

    let desired = natural
        .iter()
        .enumerate()
        .map(|(index, width)| {
            let minimum = if index == 0 {
                7
            } else if index == last {
                8
            } else {
                6
            };
            cmp::min(*width, minimum)
        })
        .collect::<Vec<_>>();
    grow_widths(&mut widths, &desired, &order, &mut remaining);
    grow_widths(&mut widths, natural, &order, &mut remaining);
    widths
}

fn grow_widths(widths: &mut [usize], targets: &[usize], order: &[usize], remaining: &mut usize) {
    while *remaining > 0 {
        let mut changed = false;
        for index in order {
            if *remaining == 0 {
                break;
            }
            if widths[*index] < targets[*index] {
                widths[*index] += 1;
                *remaining -= 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}
