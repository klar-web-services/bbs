use crate::{
    cli::{ColorChoice, OutputFormat},
    model::{ResultLine, SearchResponse},
};
use anyhow::Result;
use std::io::{self, IsTerminal, Write};
use syntect::{
    easy::HighlightLines,
    highlighting::{Style, ThemeSet},
    parsing::{SyntaxReference, SyntaxSet},
};

pub fn render(response: &SearchResponse, format: OutputFormat, color: ColorChoice) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(response)?),
        OutputFormat::Jsonl => {
            for result in &response.results {
                println!("{}", serde_json::to_string(result)?);
            }
        }
        OutputFormat::Terminal => render_terminal(response, should_color(color))?,
    }
    Ok(())
}

fn should_color(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => io::stdout().is_terminal(),
    }
}

fn render_terminal(response: &SearchResponse, color: bool) -> Result<()> {
    let syntax_set = two_face::syntax::extra_newlines();
    let themes = ThemeSet::load_defaults();
    let theme = themes
        .themes
        .get("base16-ocean.dark")
        .or_else(|| themes.themes.values().next())
        .expect("syntect includes a theme");
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    for result in &response.results {
        if color {
            writeln!(
                stdout,
                "\x1b[1;36m{}\x1b[0m  \x1b[1m{}\x1b[0m  \x1b[2m{}@{}\x1b[0m",
                result.repository,
                result.path,
                result.branch,
                &result.commit[..result.commit.len().min(12)]
            )?;
        } else {
            writeln!(
                stdout,
                "{}  {}  {}@{}",
                result.repository,
                result.path,
                result.branch,
                &result.commit[..result.commit.len().min(12)]
            )?;
        }
        let syntax = syntax_for_result_path(&syntax_set, &result.path);
        let mut highlighter = HighlightLines::new(syntax, theme);
        let width = result
            .lines
            .last()
            .map(|l| l.number.to_string().len())
            .unwrap_or(1);
        let mut previous = None;
        for line in &result.lines {
            if previous.is_some_and(|number| line.number > number + 1) {
                writeln!(stdout, "{:>width$}  …", "", width = width)?;
            }
            let marker = if line.ranges.is_empty() { " " } else { ">" };
            write!(
                stdout,
                "{} {:>width$} │ ",
                marker,
                line.number,
                width = width
            )?;
            if color {
                let source_line = format!("{}\n", line.text);
                let styled = highlighter.highlight_line(&source_line, &syntax_set)?;
                render_styled(&mut stdout, &styled, line)?;
            } else {
                writeln!(stdout, "{}", line.text)?;
            }
            previous = Some(line.number);
        }
        if color {
            writeln!(stdout, "\x1b[2;4m{}\x1b[0m\n", result.web_url)?;
        } else {
            writeln!(stdout, "{}\n", result.web_url)?;
        }
    }
    let cache = if response.cached { ", cache hit" } else { "" };
    let skipped = if response.skipped.is_empty() {
        String::new()
    } else {
        format!("; {} skipped", response.skipped.len())
    };
    writeln!(
        stdout,
        "{} results across {} repositories ({} files, {} ms{}){}{}",
        response.results.len(),
        response.repositories_searched,
        response.files_searched,
        response.elapsed_ms,
        cache,
        if response.truncated {
            "; truncated"
        } else {
            ""
        },
        skipped
    )?;
    Ok(())
}

fn syntax_for_result_path<'a>(syntax_set: &'a SyntaxSet, path: &str) -> &'a SyntaxReference {
    let path = std::path::Path::new(path);
    path.extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| syntax_set.find_syntax_by_extension(extension))
        .or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| syntax_set.find_syntax_by_extension(name))
        })
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text())
}

fn render_styled(
    writer: &mut impl Write,
    styled: &[(Style, &str)],
    line: &ResultLine,
) -> Result<()> {
    let mut offset = 0usize;
    for (style, text) in styled {
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            continue;
        }
        let mut points = vec![0, text.len()];
        for range in &line.ranges {
            if range.start > offset && range.start < offset + text.len() {
                points.push(range.start - offset);
            }
            if range.end > offset && range.end < offset + text.len() {
                points.push(range.end - offset);
            }
        }
        points.sort_unstable();
        points.dedup();
        for window in points.windows(2) {
            let start = window[0];
            let end = window[1];
            if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                continue;
            }
            let matched = line
                .ranges
                .iter()
                .any(|range| range.start < offset + end && range.end > offset + start);
            let fg = style.foreground;
            if matched {
                write!(
                    writer,
                    "\x1b[38;2;{};{};{};1;4m{}\x1b[0m",
                    fg.r,
                    fg.g,
                    fg.b,
                    &text[start..end]
                )?;
            } else {
                write!(
                    writer,
                    "\x1b[38;2;{};{};{}m{}\x1b[0m",
                    fg.r,
                    fg.g,
                    fg.b,
                    &text[start..end]
                )?;
            }
        }
        offset += text.len();
    }
    writeln!(writer)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_modern_syntaxes_without_opening_the_result_path() {
        let syntax_set = two_face::syntax::extra_newlines();

        assert_eq!(
            syntax_for_result_path(&syntax_set, "missing/source.rs").name,
            "Rust"
        );
        assert_eq!(
            syntax_for_result_path(&syntax_set, "missing/Dockerfile").name,
            "Dockerfile"
        );
        assert_eq!(
            syntax_for_result_path(&syntax_set, "missing/component.tsx").name,
            "TypeScriptReact"
        );
    }
}
