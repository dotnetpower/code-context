use crate::usage_insights::Advisory;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use unicode_width::UnicodeWidthStr;

pub(crate) struct UsageSummary<'a> {
    pub hours: f64,
    pub used_aic: f64,
    pub saved_aic: f64,
    pub tokens_saved: u64,
    pub warm_pct: Option<f64>,
    pub adoption_pct: Option<f64>,
    pub models: &'a [Value],
    pub advisories: &'a [Advisory],
}

#[derive(Clone, Copy)]
pub(crate) enum Style {
    Normal,
    Title,
    Savings,
    Muted,
    Advisory,
}

pub(crate) struct UsageDisplay {
    width: usize,
    color: bool,
}

impl UsageDisplay {
    pub(crate) fn terminal() -> Self {
        let is_terminal = std::io::stdout().is_terminal();
        let width = is_terminal
            .then(terminal_size::terminal_size)
            .flatten()
            .map(|(width, _)| usize::from(width.0))
            .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
            .filter(|width| *width > 0)
            .unwrap_or(80);
        Self {
            width: width.max(2),
            color: is_terminal
                && std::env::var_os("NO_COLOR").is_none()
                && std::env::var_os("TERM").is_none_or(|term| term != "dumb"),
        }
    }

    fn paint(&self, text: &str, style: Style) -> String {
        let code = match style {
            Style::Normal => return text.to_owned(),
            Style::Title => "1;36",
            Style::Savings => "1;32",
            Style::Muted => "90",
            Style::Advisory => "1;33",
        };
        if self.color && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    pub(crate) fn text(&self, text: &str, style: Style) -> String {
        let mut lines = Vec::new();
        for raw in text.split('\n') {
            if raw.width() <= self.width {
                lines.push(self.paint(raw, style));
                continue;
            }
            let indent = " ".repeat(
                raw.chars()
                    .take_while(|c| *c == ' ')
                    .count()
                    .min(self.width.saturating_sub(2)),
            );
            let mut line = indent.clone();
            for word in raw.split_whitespace() {
                if line.len() > indent.len() {
                    if line.width() + 1 + word.width() > self.width {
                        lines.push(self.paint(&line, style));
                        line.clone_from(&indent);
                    } else {
                        line.push(' ');
                    }
                }
                for ch in word.chars() {
                    let mut next = line.clone();
                    next.push(ch);
                    if next.width() > self.width && line.len() > indent.len() {
                        lines.push(self.paint(&line, style));
                        line.clone_from(&indent);
                    }
                    line.push(ch);
                }
            }
            if line.len() > indent.len() {
                lines.push(self.paint(&line, style));
            }
        }
        lines.join("\n")
    }

    pub(crate) fn section(&self, title: &str, style: Style) -> String {
        format!(
            "{}\n{}",
            self.text(title, style),
            self.paint(&"-".repeat(self.width), Style::Muted)
        )
    }

    pub(crate) fn field(&self, label: &str, value: &str, style: Style) -> String {
        let column = label.width().max(23) + 2;
        if column + value.width() <= self.width {
            self.text(
                &format!("{label}{}{value}", " ".repeat(column - label.width())),
                style,
            )
        } else {
            format!(
                "{}\n{}",
                self.text(label, Style::Muted),
                self.text(&format!("  {value}"), style)
            )
        }
    }

    pub(crate) fn cards(&self, title: &str, headers: &[&str], rows: &[Vec<String>]) -> String {
        let mut out = vec![self.section(title, Style::Title)];
        if rows.is_empty() {
            out.push(self.text("(none)", Style::Muted));
        }
        let mut columns: Vec<usize> = (1..headers.len()).collect();
        columns.sort_by_key(|i| {
            !(headers[*i].starts_with("Used") || headers[*i].starts_with("Est. savings"))
        });
        for row in rows {
            out.push(String::new());
            out.push(self.text(row.first().map_or("-", String::as_str), Style::Title));
            for &i in &columns {
                let style = if headers[i].starts_with("Est. savings") {
                    Style::Savings
                } else if headers[i].starts_with("Reference") {
                    Style::Muted
                } else {
                    Style::Normal
                };
                out.push(self.field(headers[i], row.get(i).map_or("-", String::as_str), style));
            }
        }
        out.join("\n")
    }

    pub(crate) fn summary(&self, summary: &UsageSummary<'_>) -> String {
        let savings_pct = if summary.used_aic + summary.saved_aic > 0.0 {
            100.0 * summary.saved_aic / (summary.used_aic + summary.saved_aic)
        } else {
            0.0
        };
        let mut out = vec![
            self.text(
                &format!("⚡ CCTX USAGE · Last {} hours", summary.hours),
                Style::Title,
            ),
            String::new(),
            self.text(
                &format!(
                    "💰 ${:.2} saved · {savings_pct:.1}% less (estimated)",
                    summary.saved_aic * 0.001
                ),
                Style::Savings,
            ),
            self.text(
                &format!(
                    "   Without cctx ~${:.2} → With cctx ${:.2}",
                    (summary.used_aic + summary.saved_aic) * 0.001,
                    summary.used_aic * 0.001
                ),
                Style::Normal,
            ),
            self.text("   USD equivalent @ $0.001/AIC; not a bill.", Style::Muted),
            String::new(),
            self.text("EFFICIENCY", Style::Title),
            self.text(
                &format!("  Tokens avoided ~{}", super::fmt_tok(summary.tokens_saved)),
                Style::Normal,
            ),
            self.text(
                &format!(
                    "  Warm hits {} · Source adoption {}",
                    super::fmt_pct(summary.warm_pct),
                    super::fmt_pct(summary.adoption_pct)
                ),
                Style::Normal,
            ),
            String::new(),
            self.text("💡 OPTIMIZATION ADVISORIES", Style::Advisory),
        ];
        let mut groups = BTreeMap::new();
        for advisory in summary.advisories {
            let group = groups
                .entry(advisory.compact_hint().1)
                .or_insert((advisory, 0));
            group.1 += 1;
        }
        let mut groups: Vec<_> = groups.into_values().collect();
        groups.sort_by_key(|(advisory, _)| (advisory.compact_hint().0, advisory.code));
        let mut shown_findings = 0;
        for (index, (advisory, count)) in groups.iter().take(3).enumerate() {
            let count_label = if *count > 1 {
                format!(" ({count} findings)")
            } else {
                String::new()
            };
            out.push(self.text(
                &format!(
                    "  {}. {}{count_label}",
                    index + 1,
                    advisory.compact_hint().1
                ),
                Style::Normal,
            ));
            shown_findings += count;
        }
        if groups.is_empty() {
            out.push(self.text("  No optimization issues detected.", Style::Muted));
        } else {
            let remaining = summary.advisories.len() - shown_findings;
            let note = if remaining > 0 {
                format!("  {remaining} more findings; full evidence: --details")
            } else {
                "  Full evidence: --details".into()
            };
            out.push(self.text(&note, Style::Muted));
        }
        out.push(String::new());
        out.push(self.text("MODELS · USD equivalent", Style::Title));
        let mut models: Vec<_> = summary.models.iter().collect();
        models.sort_by(|a, b| {
            b["used_aic"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&a["used_aic"].as_f64().unwrap_or(0.0))
        });
        let mut rows: Vec<[String; 3]> = models
            .iter()
            .take(3)
            .map(|model| {
                [
                    format!(
                        "{} ({})",
                        model["display_name"].as_str().unwrap_or("-"),
                        model["effort"].as_str().unwrap_or("-")
                    ),
                    format!("${:.2}", model["used_aic"].as_f64().unwrap_or(0.0) * 0.001),
                    format!("${:.2}", model["saved_aic"].as_f64().unwrap_or(0.0) * 0.001),
                ]
            })
            .collect();
        if models.len() > 3 {
            let used: f64 = models
                .iter()
                .skip(3)
                .map(|m| m["used_aic"].as_f64().unwrap_or(0.0))
                .sum();
            let saved: f64 = models
                .iter()
                .skip(3)
                .map(|m| m["saved_aic"].as_f64().unwrap_or(0.0))
                .sum();
            rows.push([
                format!("Other ({} models)", models.len() - 3),
                format!("${:.2}", used * 0.001),
                format!("${:.2}", saved * 0.001),
            ]);
        }
        rows.push([
            "Total".into(),
            format!("${:.2}", summary.used_aic * 0.001),
            format!("${:.2}", summary.saved_aic * 0.001),
        ]);
        out.push(self.model_table(&rows));
        out.push(String::new());
        out.push(self.text(
            &format!(
                "Full report: cctx usage --hours {} --details",
                summary.hours
            ),
            Style::Muted,
        ));
        out.join("\n")
    }

    fn model_table(&self, rows: &[[String; 3]]) -> String {
        let headers = ["Model", "Used", "Est.saved"];
        let used_width = rows
            .iter()
            .map(|r| r[1].width())
            .max()
            .unwrap_or(0)
            .max(headers[1].width());
        let saved_width = rows
            .iter()
            .map(|r| r[2].width())
            .max()
            .unwrap_or(0)
            .max(headers[2].width());
        let overhead = used_width + saved_width + 10;
        if self.width < overhead + 10 {
            return rows
                .iter()
                .map(|row| {
                    format!(
                        "{}\n{}",
                        self.text(&row[0], Style::Title),
                        self.text(
                            &format!("  {} used / {} saved", row[1], row[2]),
                            Style::Normal
                        ),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        let name_width = rows
            .iter()
            .map(|r| r[0].width())
            .max()
            .unwrap_or(0)
            .max(headers[0].width())
            .min(self.width - overhead);
        let widths = [name_width, used_width, saved_width];
        let border = |left: &str, mid: &str, right: &str| {
            self.paint(
                &format!(
                    "{left}{}{right}",
                    widths.map(|w| "─".repeat(w + 2)).join(mid)
                ),
                Style::Muted,
            )
        };
        let cells = |row: [&str; 3], savings: bool| {
            let padded: Vec<_> = row
                .iter()
                .enumerate()
                .map(|(i, value)| {
                    let pad = " ".repeat(widths[i].saturating_sub(value.width()));
                    let text = if i == 0 {
                        format!("{value}{pad}")
                    } else {
                        format!("{pad}{value}")
                    };
                    self.paint(
                        &text,
                        if savings && i == 2 {
                            Style::Savings
                        } else {
                            Style::Normal
                        },
                    )
                })
                .collect();
            format!("│ {} │", padded.join(" │ "))
        };
        let mut out = vec![
            border("┌", "┬", "┐"),
            cells(headers, false),
            border("├", "┼", "┤"),
        ];
        let name_display = Self {
            width: name_width,
            color: false,
        };
        for (index, row) in rows.iter().enumerate() {
            if rows.len() > 1 && index + 1 == rows.len() {
                out.push(border("├", "┼", "┤"));
            }
            for (line, name) in name_display
                .text(&row[0], Style::Normal)
                .lines()
                .enumerate()
            {
                let used = if line == 0 { row[1].as_str() } else { "" };
                let saved = if line == 0 { row[2].as_str() } else { "" };
                out.push(cells([name, used, saved], true));
            }
        }
        out.push(border("└", "┴", "┘"));
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_unicode_and_unbroken_names_without_losing_content() {
        let text = "  권고: 긴모델이름과경로/abcdefghijklmnopqrstuvwxyz/e\u{301}/🙂🙂";
        for width in [4, 16, 24, 40, 72] {
            let display = UsageDisplay {
                width,
                color: false,
            };
            let rendered = display.text(text, Style::Normal);
            assert!(
                rendered.lines().all(|line| line.width() <= width),
                "{rendered}"
            );
            assert_eq!(
                rendered.split_whitespace().collect::<String>(),
                text.split_whitespace().collect::<String>()
            );
        }
    }

    #[test]
    fn cards_prioritize_cost_and_wrap_every_field() {
        let headers = ["Model", "Turns", "Used equivalent", "Est. savings"];
        let rows = vec![vec![
            "A very long model name (high)".into(),
            "1,160".into(),
            "$26.13".into(),
            "$4.77 / reference $47.66 at $0.01/AIC".into(),
        ]];
        for width in [24, 40, 72, 120] {
            let display = UsageDisplay {
                width,
                color: false,
            };
            let rendered = display.cards("MODELS", &headers, &rows);
            assert!(rendered.lines().all(|line| line.width() <= width));
            assert!(rendered.find("Used equivalent").unwrap() < rendered.find("Turns").unwrap());
            assert!(rendered.contains("$4.77"));
            assert!(!rendered.contains('\x1b'));
        }
    }

    #[test]
    fn color_is_applied_after_wrapping_and_preserves_alignment() {
        let display = UsageDisplay {
            width: 40,
            color: true,
        };
        let plain = UsageDisplay {
            width: 40,
            color: false,
        };
        let value = "$4.77 saved";
        let rendered = display.field("Est. savings", value, Style::Savings);
        assert_eq!(
            rendered,
            format!(
                "\x1b[1;32m{}\x1b[0m",
                plain.field("Est. savings", value, Style::Savings)
            )
        );
        let advisory = display.text(&"warning ".repeat(12), Style::Advisory);
        assert!(advisory.lines().all(|line| line.starts_with("\x1b[1;33m")));
    }

    #[test]
    fn summary_limits_rows_and_groups_advisories_without_losing_totals() {
        let models: Vec<_> = (0..6)
            .map(|i| {
                serde_json::json!({
                    "display_name": format!("Example Model {i}"),
                    "effort": "high",
                    "used_aic": 1000.0,
                    "saved_aic": 100.0,
                })
            })
            .collect();
        let advisories = vec![
            Advisory {
                code: "model_efficiency",
                message: "full model evidence".into(),
            },
            Advisory {
                code: "client_disconnected",
                message: "full disconnect evidence".into(),
            },
            Advisory {
                code: "low_adoption_workspace",
                message: "full workspace evidence".into(),
            },
            Advisory {
                code: "model_without_cctx",
                message: "full adoption evidence".into(),
            },
            Advisory {
                code: "sequential_slicing",
                message: "full slicing evidence".into(),
            },
        ];
        let summary = UsageSummary {
            hours: 2.0,
            used_aic: 6000.0,
            saved_aic: 600.0,
            tokens_saved: 1_000_000,
            warm_pct: Some(93.6),
            adoption_pct: Some(86.2),
            models: &models,
            advisories: &advisories,
        };
        for (width, max_lines) in [(40, 46), (72, 32)] {
            let display = UsageDisplay {
                width,
                color: false,
            };
            let rendered = display.summary(&summary);
            assert!(
                rendered.lines().all(|line| line.width() <= width),
                "{rendered}"
            );
            assert!(rendered.lines().count() <= max_lines, "{rendered}");
            assert!(rendered.contains("Other (3 models)"));
            let compact: String = rendered.split_whitespace().collect();
            assert!(compact.contains("Total│$6.00│$0.60"));
            assert!(compact.contains("(2findings)"));
            assert!(compact.contains("1morefindings"));
            assert!(!rendered.contains("full model evidence"));
            assert!(!rendered.contains("Example Model 3"));
            assert!(
                rendered.find("OPTIMIZATION ADVISORIES").unwrap()
                    < rendered.find("MODELS").unwrap()
            );
            assert!(rendered.contains("1. Native client disconnected"));
            assert_eq!(
                rendered
                    .chars()
                    .filter(|ch| matches!(ch, '⚡' | '💰' | '💡'))
                    .count(),
                3
            );
        }
    }

    #[test]
    fn model_table_wraps_names_or_uses_two_line_rows_without_clipping() {
        let rows = [
            [
                "긴모델이름 (high effort)".into(),
                "$26.13".into(),
                "$4.77".into(),
            ],
            ["Total".into(), "$26.13".into(), "$4.77".into()],
        ];
        for width in [24, 40, 72] {
            let display = UsageDisplay {
                width,
                color: false,
            };
            let rendered = display.model_table(&rows);
            assert!(
                rendered.lines().all(|line| line.width() <= width),
                "{rendered}"
            );
            assert_eq!(rendered.contains('┌'), width >= 40);
            assert!(rendered.contains("$4.77"));
            let names: String = rendered
                .lines()
                .map(|line| {
                    if line.starts_with('│') {
                        line.split('│').nth(1).unwrap()
                    } else {
                        line
                    }
                })
                .collect();
            let joined: String = names
                .chars()
                .filter(|ch| !ch.is_whitespace() && *ch != '│')
                .collect();
            assert!(joined.contains("긴모델이름(higheffort)"));
        }
    }

    #[test]
    fn empty_summary_has_no_nan_or_spurious_advisories() {
        let summary = UsageSummary {
            hours: 2.0,
            used_aic: 0.0,
            saved_aic: 0.0,
            tokens_saved: 0,
            warm_pct: None,
            adoption_pct: None,
            models: &[],
            advisories: &[],
        };
        let rendered = UsageDisplay {
            width: 40,
            color: false,
        }
        .summary(&summary);
        assert!(rendered.contains("No optimization issues detected."));
        assert!(rendered.contains("0.0% less"));
        assert!(!rendered.contains("NaN"));
        assert!(!rendered.contains('\x1b'));
        assert!(rendered.lines().all(|line| line.width() <= 40));
    }
}
