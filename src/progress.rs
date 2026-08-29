use std::cell::Cell;
use std::time::Duration;

use console::Term;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::display::{pad_right, truncate, width as display_width};

pub(crate) struct ProgressManager {
    multi: MultiProgress,
    overall: ProgressBar,
    styles: ProgressStyles,
    enabled: bool,
}

pub(crate) struct TaskProgress {
    bar: ProgressBar,
    spinner_style: ProgressStyle,
    unknown_download_style: ProgressStyle,
    profile: String,
    name: String,
    prefix_width: Cell<usize>,
    download_label_width: Cell<usize>,
    terminal_width: Cell<usize>,
    determinate: Cell<bool>,
    enabled: bool,
}

#[derive(Clone)]
struct ProgressStyles {
    overall: ProgressStyle,
    spinner: ProgressStyle,
    unknown_download: ProgressStyle,
}

impl ProgressManager {
    pub(crate) fn new(requested: bool, total: usize) -> Self {
        let terminal = Term::stderr();
        let enabled = requested && terminal.is_term();
        let width = terminal.size().1 as usize;
        let styles = styles_for_width(width);
        let multi = MultiProgress::new();
        let overall = if enabled {
            let bar = multi.add(ProgressBar::new(total as u64));
            bar.set_style(styles.overall.clone());
            bar.set_message(format!("updating tools ({total} total)"));
            bar
        } else {
            ProgressBar::hidden()
        };
        Self {
            multi,
            overall,
            styles,
            enabled,
        }
    }

    pub(crate) fn task(&self, profile: &str, name: &str) -> TaskProgress {
        let prefix = task_prefix(profile, name, current_terminal_width());
        let prefix_width = display_width(&prefix);
        let bar = if self.enabled {
            let bar = self.multi.add(ProgressBar::new_spinner());
            bar.set_style(self.styles.spinner.clone());
            bar.set_prefix(prefix);
            bar.enable_steady_tick(Duration::from_millis(100));
            bar
        } else {
            ProgressBar::hidden()
        };
        TaskProgress {
            bar,
            spinner_style: self.styles.spinner.clone(),
            unknown_download_style: self.styles.unknown_download.clone(),
            profile: profile.to_owned(),
            name: name.to_owned(),
            prefix_width: Cell::new(prefix_width),
            download_label_width: Cell::new(0),
            terminal_width: Cell::new(0),
            determinate: Cell::new(false),
            enabled: self.enabled,
        }
    }

    pub(crate) fn complete(&self, task: &TaskProgress) {
        // 先移除任务，避免清理任务和更新总进度连续触发两次重绘。
        if self.enabled {
            self.multi.remove(&task.bar);
            self.overall.inc(1);
        }
        task.bar.finish_and_clear();
    }

    pub(crate) fn finish(&self) {
        self.overall.finish_and_clear();
        if self.enabled {
            self.multi.remove(&self.overall);
            let _ = self.multi.clear();
        }
    }
}

impl TaskProgress {
    pub(crate) fn stage(&self, stage: &str) {
        self.update_prefix(current_terminal_width());
        self.determinate.set(false);
        self.bar.reset();
        self.bar.unset_length();
        self.bar.set_style(self.spinner_style.clone());
        self.bar.set_message(stage.to_owned());
    }

    pub(crate) fn download(
        &self,
        artifact: usize,
        artifacts: usize,
        filename: &str,
        total: Option<u64>,
    ) {
        self.bar.reset();
        let label = download_label(artifact, artifacts, filename);
        self.bar.set_message(label.clone());
        let terminal_width = current_terminal_width();
        self.update_prefix(terminal_width);
        if let Some(total) = total {
            let label_width = display_width(&label);
            self.terminal_width.set(terminal_width);
            self.download_label_width.set(label_width);
            self.determinate.set(true);
            self.bar.set_length(total);
            self.bar.set_style(download_style(
                terminal_width,
                self.prefix_width.get(),
                label_width,
            ));
        } else {
            self.determinate.set(false);
            self.bar.unset_length();
            self.bar.set_style(self.unknown_download_style.clone());
        }
    }

    pub(crate) fn inc(&self, bytes: u64) {
        self.bar.inc(bytes);
        if self.enabled && self.determinate.get() {
            let terminal_width = current_terminal_width();
            if terminal_width != self.terminal_width.get() {
                self.terminal_width.set(terminal_width);
                self.update_prefix(terminal_width);
                self.bar.set_style(download_style(
                    terminal_width,
                    self.prefix_width.get(),
                    self.download_label_width.get(),
                ));
            }
        }
    }

    pub(crate) fn set_position(&self, bytes: u64) {
        self.bar.set_position(bytes);
    }

    fn update_prefix(&self, terminal_width: usize) {
        let prefix = task_prefix(&self.profile, &self.name, terminal_width);
        self.prefix_width.set(display_width(&prefix));
        self.bar.set_prefix(prefix);
    }
}

fn download_label(artifact: usize, artifacts: usize, filename: &str) -> String {
    if artifacts > 1 {
        format!("{filename} ({artifact}/{artifacts})")
    } else {
        filename.to_owned()
    }
}

fn styles_for_width(width: usize) -> ProgressStyles {
    if width >= 100 {
        ProgressStyles {
            overall: style(
                "{spinner:.green} {msg} [{bar:32.cyan/blue}] {pos}/{len} {elapsed_precise}",
            ),
            spinner: style("{spinner:.green} {prefix:.cyan} {msg:14!} {elapsed_precise}"),
            unknown_download: style("{spinner:.green} {prefix:.cyan} {wide_msg} {bytes}"),
        }
    } else if width >= 70 {
        ProgressStyles {
            overall: style("{spinner:.green} {msg} [{bar:20.cyan/blue}] {pos}/{len}"),
            spinner: style("{spinner:.green} {prefix:.cyan} {msg:10!} {elapsed_precise}"),
            unknown_download: style("{spinner:.green} {prefix:.cyan} {wide_msg} {bytes}"),
        }
    } else {
        ProgressStyles {
            overall: style("{spinner:.green} {msg} {pos}/{len}"),
            spinner: style("{spinner:.green} {prefix:.cyan} {msg}"),
            unknown_download: style("{spinner:.green} {prefix:.cyan} {wide_msg} {bytes}"),
        }
    }
}

fn download_style(width: usize, prefix_width: usize, label_width: usize) -> ProgressStyle {
    if width >= 100 {
        let (message_width, bar_width) =
            split_download_width(width.saturating_sub(prefix_width + 40), label_width, 12);
        style(&format!(
            "{{prefix:.cyan}} {{msg:{message_width}!}} [{{bar:{bar_width}.green/black}}] {{bytes}}/{{total_bytes}} {{eta}}"
        ))
    } else {
        let usable = width.saturating_sub(prefix_width + 9);
        if usable < 16 {
            return style("{prefix:.cyan} [{wide_bar:.green/black}] {percent:>3}%");
        }
        let (message_width, bar_width) = split_download_width(usable, label_width, 8);
        style(&format!(
            "{{prefix:.cyan}} {{msg:{message_width}!}} [{{bar:{bar_width}.green/black}}] {{percent:>3}}%"
        ))
    }
}

fn split_download_width(usable: usize, label_width: usize, minimum: usize) -> (usize, usize) {
    let message_limit = (usable.saturating_mul(2) / 3).min(usable.saturating_sub(minimum));
    let message_width = label_width.clamp(minimum, message_limit.max(minimum));
    let bar_width = usable.saturating_sub(message_width).max(minimum);
    (message_width, bar_width)
}

fn task_prefix(profile: &str, name: &str, terminal_width: usize) -> String {
    let budget = prefix_budget(terminal_width);
    let content_budget = budget.saturating_sub(5);
    let minimum_name_width = if budget >= 22 { 6 } else { 3 };
    let profile_budget = display_width(profile)
        .min(if budget >= 30 { 12 } else { 10 })
        .min(content_budget.saturating_sub(minimum_name_width))
        .max(1);
    let profile = truncate(profile, profile_budget);
    let name_budget = content_budget
        .saturating_sub(display_width(&profile))
        .max(1);
    let name = truncate(name, name_budget);
    let mut prefix = pad_right(&format!("[{profile}] {name}"), budget.saturating_sub(2));
    prefix.push_str(" ›");
    prefix
}

fn prefix_budget(terminal_width: usize) -> usize {
    if terminal_width >= 140 {
        36
    } else if terminal_width >= 100 {
        30
    } else if terminal_width >= 70 {
        22
    } else {
        16
    }
}

fn current_terminal_width() -> usize {
    Term::stderr().size().1 as usize
}

fn style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template)
        .expect("static progress template")
        .progress_chars("=>-")
}

#[cfg(test)]
mod tests {
    use super::{download_label, prefix_budget, split_download_width, task_prefix};
    use crate::display::width as display_width;

    #[test]
    fn adapts_profile_and_name_space_to_terminal_width() {
        assert_eq!(prefix_budget(160), 36);
        assert_eq!(prefix_budget(120), 30);
        assert_eq!(prefix_budget(80), 22);
        assert_eq!(prefix_budget(50), 16);

        assert_eq!(
            task_prefix("windows", "Behinder", 80),
            "[windows] Behinder   ›"
        );
        assert_eq!(task_prefix("windows", "Behinder", 50), "[windows] Beh… ›");
        assert_eq!(display_width(&task_prefix("windows", "Behinder", 120)), 30);
        assert_eq!(
            display_width(&task_prefix("企业版", "哈希值批量计算器", 80)),
            22
        );
    }

    #[test]
    fn download_label_shows_the_current_filename_and_position() {
        assert_eq!(
            download_label(3, 24, "frida-server-17.17.0-linux-arm64.xz"),
            "frida-server-17.17.0-linux-arm64.xz (3/24)"
        );
        assert_eq!(download_label(1, 1, "tool.zip"), "tool.zip");
    }

    #[test]
    fn expands_the_download_bar_with_the_terminal() {
        let narrow = split_download_width(60, 40, 12);
        let wide = split_download_width(140, 40, 12);

        assert_eq!(narrow.0, 40);
        assert_eq!(wide.0, 40);
        assert!(wide.1 > narrow.1);
    }
}
