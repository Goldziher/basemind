//! The interactive capability prompt of `basemind init`: one yes/no question per [`Capability`].

use std::io::{BufRead, Write};

use anyhow::{Context, Result};

use super::init_rules::Capability;

/// Interactive yes/no prompt per capability. Hand-rolled over stdin — no new crate dependency.
/// A blank answer accepts the default (yes).
pub(super) fn prompt_capabilities() -> Result<Vec<Capability>> {
    read_capability_answers(&mut std::io::stdin().lock(), &mut std::io::stdout())
}

/// Ask `output` about each capability in turn and read the answers from `input`. A blank answer
/// accepts the default (yes); EOF accepts the default for that capability and every later one, so
/// closing stdin mid-prompt selects what accepting every remaining default would.
fn read_capability_answers(input: &mut impl BufRead, output: &mut impl Write) -> Result<Vec<Capability>> {
    writeln!(output, "Select basemind capabilities to advertise (Y/n, blank = yes):").context("write prompt")?;
    let mut selected = Vec::new();
    for (index, cap) in Capability::ALL.into_iter().enumerate() {
        write!(output, "  {} [Y/n] ", cap.label()).context("write prompt")?;
        output.flush().context("flush prompt")?;
        let mut line = String::new();
        if input.read_line(&mut line).context("read stdin")? == 0 {
            selected.extend_from_slice(&Capability::ALL[index..]);
            break;
        }
        let answer = line.trim().to_ascii_lowercase();
        if answer.is_empty() || answer == "y" || answer == "yes" {
            selected.push(cap);
        }
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers read before EOF are honoured and every capability after it takes its default (yes),
    /// so closing stdin mid-prompt matches accepting every remaining default.
    #[test]
    fn capability_prompt_eof_accepts_remaining_defaults() {
        let all = Capability::ALL;
        let blanks = "\n".repeat(all.len());
        let cases: [(&str, Vec<Capability>); 5] = [
            ("", all.to_vec()),
            (&blanks, all.to_vec()),
            ("n\n", all[1..].to_vec()),
            ("n\ny\nno", [&all[1..2], &all[3..]].concat()),
            ("y\nn\nn\n", [&all[..1], &all[3..]].concat()),
        ];
        for (input, expected) in cases {
            let selected =
                read_capability_answers(&mut input.as_bytes(), &mut Vec::new()).expect("prompt reads answers");
            assert_eq!(selected, expected, "input {input:?}");
        }
    }

    /// EOF ends the prompt: no capability after it is asked about.
    #[test]
    fn capability_prompt_stops_asking_at_eof() {
        let mut output = Vec::new();
        read_capability_answers(&mut "n\n".as_bytes(), &mut output).expect("prompt reads answers");
        let expected = format!(
            "Select basemind capabilities to advertise (Y/n, blank = yes):\n  {} [Y/n]   {} [Y/n] ",
            Capability::ALL[0].label(),
            Capability::ALL[1].label()
        );
        assert_eq!(String::from_utf8(output).expect("utf-8 prompt"), expected);
    }
}
