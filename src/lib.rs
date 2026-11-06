//! Link Wikidata IDs and verified chemical/taxon names in Markdown-like text.
use pulldown_cmark::{Event, Parser, Tag, TagEnd};
use regex::Regex;
use serde_json::Value;
use std::ops::Range;

const API: &str = "https://www.wikidata.org/w/api.php";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Chemical,
    Taxon,
}

impl Kind {
    fn property(self) -> &'static str {
        match self {
            Self::Chemical => "P235", // InChIKey
            Self::Taxon => "P225",    // taxon name
        }
    }
}

#[derive(Debug)]
pub struct Target {
    pub name: String,
    pub kind: Kind,
    pub id: Option<String>,
}

/// Search exact English labels/aliases; require a domain-specific Wikidata claim.
/// Uncertain results remain unlinked, with candidate URLs in the review log.
pub fn resolve(name: &str, kind: Kind) -> (Option<String>, Vec<String>) {
    let response = ureq::get(API)
        .query("action", "wbsearchentities")
        .query("search", name)
        .query("language", "en")
        .query("type", "item")
        .query("limit", "50")
        .query("format", "json")
        .header("User-Agent", "moteado/0.1 (Wikidata text linker)")
        .call()
        .and_then(|mut r| r.body_mut().read_json::<Value>());
    let search = match response {
        Ok(v) => v,
        Err(e) => return (None, vec![format!("ERROR {name:?}: search failed: {e}")]),
    };
    let Some(rows) = search["search"].as_array() else {
        return (
            None,
            vec![format!("ERROR {name:?}: invalid search response")],
        );
    };
    let ids: Vec<&str> = rows
        .iter()
        .filter(|row| {
            row["match"]["text"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(name))
                || row["label"]
                    .as_str()
                    .is_some_and(|s| s.eq_ignore_ascii_case(name))
        })
        .filter_map(|row| row["id"].as_str())
        .filter(|id| valid_id(id, 'Q'))
        .collect();
    if ids.is_empty() {
        return (
            None,
            vec![format!("REVIEW {name:?}: no exact Wikidata label/alias")],
        );
    }
    let response = ureq::get(API)
        .query("action", "wbgetentities")
        .query("ids", &ids.join("|"))
        .query("props", "claims")
        .query("format", "json")
        .header("User-Agent", "moteado/0.1 (Wikidata text linker)")
        .call()
        .and_then(|mut r| r.body_mut().read_json::<Value>());
    let entities = match response {
        Ok(v) => v,
        Err(e) => {
            return (
                None,
                vec![format!("ERROR {name:?}: entity lookup failed: {e}")],
            );
        }
    };
    let mut candidates: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| {
            let claims = &entities["entities"][id]["claims"];
            if kind == Kind::Taxon {
                claims["P225"].as_array().is_some_and(|values| {
                    values.iter().any(|v| {
                        v["mainsnak"]["datavalue"]["value"]
                            .as_str()
                            .is_some_and(|s| s.eq_ignore_ascii_case(name))
                    })
                })
            } else {
                claims[kind.property()]
                    .as_array()
                    .is_some_and(|v| !v.is_empty())
            }
        })
        .collect();
    let mut notes = Vec::new();
    // A stereoisomer group can share both a label and an InChIKey with a
    // particular compound. Prefer an individually described compound.
    if kind == Kind::Chemical && candidates.len() > 1 {
        let individual: Vec<&str> = candidates
            .iter()
            .copied()
            .filter(|id| {
                !entities["entities"][id]["claims"]["P31"]
                    .as_array()
                    .is_some_and(|claims| {
                        claims
                            .iter()
                            .any(|v| v["mainsnak"]["datavalue"]["value"]["id"] == "Q59199015")
                    })
            })
            .collect();
        if individual.len() == 1 {
            notes.push(format!(
                "REVIEW {name:?}: chose {} over stereoisomer group(s): {}",
                individual[0],
                candidates
                    .iter()
                    .filter(|id| **id != individual[0])
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            candidates = individual;
        }
    }
    if rows.len() == 50 {
        notes.push(format!(
            "REVIEW {name:?}: search capped at 50 results; no automatic link"
        ));
        return (None, notes);
    }
    match candidates.as_slice() {
        [id] => (Some((*id).to_string()), notes),
        [] => (
            None,
            vec![format!(
                "REVIEW {name:?}: no exact {:?} candidate with {}",
                kind,
                kind.property()
            )],
        ),
        _ => (
            None,
            vec![format!(
                "REVIEW {name:?}: ambiguous {:?} candidates: {}",
                kind,
                candidates.join(", ")
            )],
        ),
    }
}

/// Resolve auto-discovered candidates in indexed Wikidata SPARQL batches.
/// Unlike independent API searches, each batch uses one request and returns
/// every exact match, including competing entity records.
pub fn resolve_discovered(
    names: &[String],
    kind: Kind,
    mut progress: impl FnMut(usize),
) -> (Vec<Target>, Vec<String>) {
    let mut targets = Vec::new();
    let mut notes = Vec::new();
    for chunk in names.chunks(30) {
        let mut query = String::from("SELECT ?name ?item ?group WHERE { VALUES ");
        match kind {
            Kind::Taxon => {
                query.push_str("?name { ");
                for name in chunk {
                    query.push_str(&format!("{name:?} "));
                }
                query.push_str("} ?item wdt:P225 ?name . }");
            }
            Kind::Chemical => {
                query.push_str("(?name ?label) { ");
                for name in chunk {
                    let mut chars = name.chars();
                    let label = chars
                        .next()
                        .map(|first| first.to_lowercase().chain(chars).collect::<String>())
                        .unwrap_or_default();
                    query.push_str(&format!("({name:?} {label:?}@en) "));
                }
                query.push_str("} ?item (rdfs:label|skos:altLabel) ?label ; wdt:P235 ?key . ");
                query.push_str("BIND(EXISTS { ?item wdt:P31 wd:Q59199015 } AS ?group) }");
            }
        }
        let response = ureq::get("https://query.wikidata.org/sparql")
            .query("query", &query)
            .header("Accept", "application/sparql-results+json")
            .header("User-Agent", "moteado/0.1 (Wikidata text linker)")
            .call()
            .and_then(|mut r| r.body_mut().read_json::<Value>());
        let data = match response {
            Ok(data) => data,
            Err(e) => {
                notes.push(format!("ERROR auto {kind:?}: batch lookup failed: {e}"));
                break;
            }
        };
        let Some(bindings) = data["results"]["bindings"].as_array() else {
            notes.push(format!("ERROR auto {kind:?}: invalid batch response"));
            break;
        };
        let mut hits: Vec<Vec<(String, bool)>> = vec![Vec::new(); chunk.len()];
        for row in bindings {
            let (Some(name), Some(uri)) =
                (row["name"]["value"].as_str(), row["item"]["value"].as_str())
            else {
                continue;
            };
            let Some((i, _)) = chunk
                .iter()
                .enumerate()
                .find(|(_, value)| value.as_str() == name)
            else {
                continue;
            };
            let id = uri.rsplit('/').next().unwrap_or("");
            if valid_id(id, 'Q') && !hits[i].iter().any(|(existing, _)| existing == id) {
                hits[i].push((id.to_string(), row["group"]["value"] == "true"));
            }
        }
        for (name, mut candidates) in chunk.iter().zip(hits) {
            if kind == Kind::Chemical && candidates.len() > 1 {
                let individual: Vec<_> = candidates.iter().filter(|(_, group)| !group).collect();
                if individual.len() == 1 {
                    let chosen = individual[0].0.clone();
                    notes.push(format!(
                        "REVIEW {name:?}: chose {chosen} over stereoisomer group(s): {}",
                        candidates
                            .iter()
                            .filter(|(id, _)| id != &chosen)
                            .map(|(id, _)| id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                    candidates.retain(|(id, _)| id == &chosen);
                }
            }
            match candidates.as_slice() {
                [(id, _)] => targets.push(Target {
                    name: name.clone(),
                    kind,
                    id: Some(id.clone()),
                }),
                [] => {} // A heuristic candidate without an exact property-backed match.
                _ => notes.push(format!(
                    "REVIEW {name:?}: ambiguous {kind:?} candidates: {}",
                    candidates
                        .iter()
                        .map(|(id, _)| id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        }
        progress(chunk.len());
    }
    (targets, notes)
}

fn valid_id(id: &str, prefix: char) -> bool {
    id.starts_with(prefix)
        && id
            .as_bytes()
            .get(1)
            .is_some_and(|c| (b'1'..=b'9').contains(c))
        && id.as_bytes()[2..].iter().all(u8::is_ascii_digit)
}

// Detect fences even where a Markdown parser treats a fence without a blank
// preceding line as ordinary paragraph text.
fn fenced_ranges(input: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut open: Option<(u8, usize, usize)> = None;
    let mut offset = 0;
    for line in input.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let bytes = trimmed.as_bytes();
        if indent <= 3 && matches!(bytes.first(), Some(b'`' | b'~')) {
            let marker = bytes[0];
            let count = bytes.iter().take_while(|b| **b == marker).count();
            match open {
                None if count >= 3 => open = Some((marker, count, offset)),
                Some((m, n, start))
                    if marker == m && count >= n && trimmed[count..].trim().is_empty() =>
                {
                    ranges.push(start..offset + line.len());
                    open = None;
                }
                _ => {}
            }
        }
        offset += line.len();
    }
    if let Some((_, _, start)) = open {
        ranges.push(start..input.len());
    }
    ranges
}

fn html_tag_depth(tag: &str, depth: &mut usize) {
    let tag = tag.trim();
    if tag.starts_with("</") {
        *depth = depth.saturating_sub(1);
    } else if tag.starts_with('<')
        && tag.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic)
        && !tag.ends_with("/>")
    {
        let name = tag[1..]
            .split(|c: char| c.is_whitespace() || c == '>')
            .next()
            .unwrap_or("");
        if !["br", "hr", "img", "input", "meta", "link", "wbr"]
            .contains(&name.to_ascii_lowercase().as_str())
        {
            *depth += 1;
        }
    }
}

/// Propose likely names from visible prose without querying Wikidata for every
/// word. These are only candidates: `resolve` must verify each before linking.
/// Taxa favor italicized binomials, then plain binomials (including table cells).
pub fn discover(input: &str, kind: Kind) -> Vec<String> {
    let pattern = match kind {
        Kind::Taxon => r"\b[A-Z][a-z]{3,} [a-z][a-z-]{3,}\b",
        Kind::Chemical => {
            r"(?i:\b[a-z][a-z-]{4,} acid\b)|\b[A-Z][a-z]{5,} [A-Z]\b|(?i:\b[a-z][a-z-]{4,}(?:ine|one|ol|in)\b)"
        }
    };
    let re = Regex::new(pattern).expect("static candidate regex");
    let url_re = Regex::new(r"(?i)\b(?:https?://|www\.)[^\s<>()]+").expect("static regex");
    let fenced = fenced_ranges(input);
    let mut italic = Vec::new();
    let mut plain = Vec::new();
    let mut emphasis = 0usize;
    let mut protected = 0usize;
    let mut html_depth = 0usize;
    for (event, range) in Parser::new(input).into_offset_iter() {
        match event {
            Event::Start(Tag::Link { .. } | Tag::Image { .. }) => protected += 1,
            Event::End(TagEnd::Link | TagEnd::Image) => protected -= 1,
            Event::InlineHtml(tag) => html_tag_depth(&tag, &mut html_depth),
            Event::Start(Tag::Emphasis) => emphasis += 1,
            Event::End(TagEnd::Emphasis) => emphasis -= 1,
            Event::Text(_) if protected == 0 && html_depth == 0 => {
                let raw = &input[range.clone()];
                let urls: Vec<_> = url_re.find_iter(raw).map(|m| m.range()).collect();
                for m in re.find_iter(raw) {
                    if urls
                        .iter()
                        .any(|url| m.start() < url.end && m.end() > url.start)
                        || fenced.iter().any(|f| {
                            range.start + m.start() < f.end && range.start + m.end() > f.start
                        })
                    {
                        continue;
                    }
                    let candidate = m.as_str().to_string();
                    if kind == Kind::Taxon && emphasis > 0 {
                        italic.push(candidate);
                    } else {
                        plain.push(candidate);
                    }
                }
            }
            _ => {}
        }
    }
    let mut seen = std::collections::HashSet::new();
    italic
        .into_iter()
        .chain(plain)
        .filter(|name| seen.insert(name.to_ascii_lowercase()))
        .collect()
}

/// Preserve the source verbatim outside new Markdown links. Standalone IDs in
/// inline code become links with code-styled labels; fenced code stays literal.
pub fn link_text(input: &str, targets: &[Target]) -> (String, Vec<String>) {
    let id_re = Regex::new(r"\b[QP][1-9][0-9]*\b").expect("static regex");
    let url_re = Regex::new(r"(?i)\b(?:https?://|www\.)[^\s<>()]+").expect("static regex");
    let names: Vec<Regex> = targets
        .iter()
        .map(|t| Regex::new(&format!(r"(?i)\b{}\b", regex::escape(&t.name))).expect("escaped name"))
        .collect();
    let mut replacements: Vec<(Range<usize>, String)> = Vec::new();
    let mut link_depth = 0usize;
    let mut html_depth = 0usize;
    let mut seen = vec![false; targets.len()];
    let fenced = fenced_ranges(input);
    for (event, range) in Parser::new(input).into_offset_iter() {
        match event {
            Event::Start(Tag::Link { .. } | Tag::Image { .. }) => link_depth += 1,
            Event::End(TagEnd::Link | TagEnd::Image) => link_depth -= 1,
            Event::InlineHtml(tag) => html_tag_depth(&tag, &mut html_depth),
            Event::Code(_)
                if link_depth == 0
                    && html_depth == 0
                    && !fenced
                        .iter()
                        .any(|f| range.start < f.end && range.end > f.start) =>
            {
                let raw = &input[range.clone()];
                let ticks = raw.bytes().take_while(|b| *b == b'`').count();
                if ticks > 0
                    && raw.len() >= ticks * 2
                    && raw.as_bytes()[raw.len() - ticks..]
                        .iter()
                        .all(|b| *b == b'`')
                {
                    let content = raw[ticks..raw.len() - ticks].trim();
                    let id = content.strip_prefix("wd:").unwrap_or(content);
                    if valid_id(id, 'Q') || valid_id(id, 'P') {
                        replacements.push((
                            range,
                            format!("[{raw}](https://www.wikidata.org/wiki/{id})"),
                        ));
                    }
                }
            }
            Event::Text(_) if link_depth == 0 && html_depth == 0 => {
                let raw = &input[range.clone()];
                let urls: Vec<_> = url_re.find_iter(raw).map(|m| m.range()).collect();
                let mut matches: Vec<(Range<usize>, Option<usize>)> =
                    id_re.find_iter(raw).map(|m| (m.range(), None)).collect();
                for (index, re) in names.iter().enumerate() {
                    matches.extend(re.find_iter(raw).map(|m| (m.range(), Some(index))));
                }
                matches
                    .sort_by_key(|(r, name)| (r.start, std::cmp::Reverse(r.end), name.is_none()));
                let mut end = 0;
                for (r, name) in matches {
                    if r.start < end
                        || urls
                            .iter()
                            .any(|url| r.start < url.end && r.end > url.start)
                        || fenced
                            .iter()
                            .any(|f| range.start + r.start < f.end && range.start + r.end > f.start)
                    {
                        continue;
                    }
                    let label = &raw[r.clone()];
                    let id = match name {
                        Some(i) => {
                            seen[i] = true;
                            targets[i].id.as_deref()
                        }
                        None => Some(label),
                    };
                    if let Some(id) = id {
                        replacements.push((
                            range.start + r.start..range.start + r.end,
                            format!("[{label}](https://www.wikidata.org/wiki/{id})"),
                        ));
                    }
                    end = r.end;
                }
            }
            _ => {}
        }
    }
    let mut out = String::with_capacity(input.len() + replacements.len() * 55);
    let mut notes = Vec::with_capacity(replacements.len() + targets.len());
    let (mut line, mut column) = (1usize, 1usize);
    let mut end = 0;
    for (range, link) in replacements {
        let unchanged = &input[end..range.start];
        out.push_str(unchanged);
        for ch in unchanged.chars() {
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        let original = &input[range.clone()];
        notes.push(format!("LINK {line}:{column} {original:?} -> {link}"));
        out.push_str(&link);
        for ch in original.chars() {
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
        }
        end = range.end;
    }
    out.push_str(&input[end..]);
    notes.extend(
        targets
            .iter()
            .zip(seen)
            .filter(|(_, seen)| !seen)
            .map(|(target, _)| {
                format!(
                    "REVIEW {:?}: name {:?} not found in linkable text",
                    target.kind, target.name
                )
            }),
    );
    (out, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_protected_markdown() {
        let input = "Q27133 P235 Q0 AQ12 wd:Q5 [Q6](https://example.org/Q7) `Q8` ![Q9](img.png) https://www.wikidata.org/wiki/Q10 <span>Q12</span>\n~~~\nQ11\n~~~\n";
        let (out, _) = link_text(input, &[]);
        assert!(out.contains("[Q27133](https://www.wikidata.org/wiki/Q27133)"));
        assert!(out.contains("[P235](https://www.wikidata.org/wiki/P235)"));
        assert!(out.contains("wd:[Q5](https://www.wikidata.org/wiki/Q5)"));
        assert!(out.contains("[Q6](https://example.org/Q7) [`Q8`](https://www.wikidata.org/wiki/Q8) ![Q9](img.png) https://www.wikidata.org/wiki/Q10 <span>Q12</span>"));
        assert!(out.contains("~~~\nQ11\n~~~"));
        assert_eq!(link_text(&out, &[]).0, out);
    }

    #[test]
    fn manuscript_inline_code_ids_are_clickable_without_touching_other_code() {
        let input = "On the Wikidata item (`Q116482353`), the qualifier (`P2241`) and `wd:Q27108137` appear beside `GBKDFEWPGPDDBU-HPEFMPOCSA-N` and `{python} g['B_hits']`.\n\n```{python}\nQ116482353 P2241\n```\n";
        let (out, _) = link_text(input, &[]);
        assert!(out.contains("[`Q116482353`](https://www.wikidata.org/wiki/Q116482353)"));
        assert!(out.contains("[`P2241`](https://www.wikidata.org/wiki/P2241)"));
        assert!(out.contains("[`wd:Q27108137`](https://www.wikidata.org/wiki/Q27108137)"));
        assert!(out.contains("`GBKDFEWPGPDDBU-HPEFMPOCSA-N` and `{python} g['B_hits']`"));
        assert!(out.ends_with("```{python}\nQ116482353 P2241\n```\n"));
        assert_eq!(link_text(&out, &[]).0, out);
    }

    #[test]
    fn exact_names_and_boundaries() {
        let targets = [
            Target {
                name: "quillaic acid".into(),
                kind: Kind::Chemical,
                id: Some("Q27108137".into()),
            },
            Target {
                name: "Quillaja saponaria".into(),
                kind: Kind::Taxon,
                id: None,
            },
        ];
        let (out, _) = link_text(
            "Quillaic acid, quillaic acids; *Quillaja saponaria*",
            &targets,
        );
        assert!(out.starts_with(
            "[Quillaic acid](https://www.wikidata.org/wiki/Q27108137), quillaic acids"
        ));
        assert!(out.ends_with("*Quillaja saponaria*"));
    }

    #[test]
    fn tabular_rows_and_unresolved_names() {
        let targets = [
            Target {
                name: "Quillaja saponaria".into(),
                kind: Kind::Taxon,
                id: Some("Q2382775".into()),
            },
            Target {
                name: "quillaic acid".into(),
                kind: Kind::Chemical,
                id: None,
            },
        ];
        let input = "name,id\n\"Quillaja saponaria\",Q27133\nquillaic acid\tP235\n";
        let (out, notes) = link_text(input, &targets);
        assert_eq!(
            out,
            "name,id\n\"[Quillaja saponaria](https://www.wikidata.org/wiki/Q2382775)\",[Q27133](https://www.wikidata.org/wiki/Q27133)\nquillaic acid\t[P235](https://www.wikidata.org/wiki/P235)\n"
        );
        assert_eq!(notes.iter().filter(|n| n.starts_with("LINK ")).count(), 3);
        assert!(notes.iter().any(|n| n.starts_with("LINK 2:2 \"Quillaja saponaria\" -> [Quillaja saponaria](https://www.wikidata.org/wiki/Q2382775)")));
        assert!(!notes.iter().any(|n| n.starts_with("REVIEW ")));
    }

    #[test]
    fn logs_each_occurrence_with_original_unicode_line_and_column() {
        let (out, notes) = link_text("α Q2\n`P3` Q2\n", &[]);
        assert_eq!(
            notes,
            [
                "LINK 1:3 \"Q2\" -> [Q2](https://www.wikidata.org/wiki/Q2)",
                "LINK 2:1 \"`P3`\" -> [`P3`](https://www.wikidata.org/wiki/P3)",
                "LINK 2:6 \"Q2\" -> [Q2](https://www.wikidata.org/wiki/Q2)",
            ]
        );
        assert!(link_text(&out, &[]).1.is_empty());
    }

    #[test]
    fn discovers_likely_names_without_touching_code_or_existing_links() {
        let text = "Hypericum elodeoides, then *Quillaja saponaria*, quillaic acid, \
            Hyperelodione D and piperine. [*Rosmarinus officinalis*](https://example.org) \
            `Sedum rosea` <span>Magnolia officinalis</span>\n\n```{python}\nSalvia rosmarinus; carnosic acid\n```\n";
        let taxa = discover(text, Kind::Taxon);
        assert_eq!(taxa.first().map(String::as_str), Some("Quillaja saponaria"));
        assert!(taxa.contains(&"Hypericum elodeoides".to_string()));
        assert!(!taxa.contains(&"Rosmarinus officinalis".to_string()));
        assert!(!taxa.contains(&"Salvia rosmarinus".to_string()));
        assert!(!taxa.contains(&"Magnolia officinalis".to_string()));
        assert!(
            discover("name,id\n\"Quillaja saponaria\",Q2382775\n", Kind::Taxon)
                .contains(&"Quillaja saponaria".to_string())
        );
        let chemicals = discover(text, Kind::Chemical);
        for name in ["quillaic acid", "Hyperelodione D", "piperine"] {
            assert!(
                chemicals.iter().any(|c| c.eq_ignore_ascii_case(name)),
                "{name}"
            );
        }
        assert!(
            !chemicals
                .iter()
                .any(|c| c.eq_ignore_ascii_case("carnosic acid"))
        );
    }
}
