//! `gz completion <shell>`: a small script that asks `gz __complete` for
//! the candidates at the cursor, so completion offers real project, job,
//! table and branch names as well as subcommands and flags.
//!
//! The protocol: `gz __complete <shell> <words left of the cursor…>` with
//! the word under the cursor in `GZ_COMPLETE_WORD` (bash sends the whole
//! line up to the cursor in `GZ_COMPLETE_LINE` instead, since its own word
//! splitting breaks at `=` and `:`). Each answer line is `value<TAB>about`
//! (bash: the value alone); a value ending in `.` is a step of a
//! `WAREHOUSE.DATABASE.TABLE` name and takes no trailing space.

use anyhow::{bail, Result};

pub const SETUP_HELP: &str = "\
Completion offers subcommands, flags and real values: project, job, dataset \
and other names, warehouses and WAREHOUSE.DATABASE.TABLE one part at a time, \
a table's branches and tags (--at, --from, --to), profiles, compute sizes and \
runtimes. Names come from a small per-profile cache in the gz config folder \
(completion/<profile>.json, names only), refreshed in the background; when \
you are not signed in nothing is completed.

Setup, one line in your shell's startup file:
  bash        eval \"$(gz completion bash)\"                       ~/.bashrc
  zsh         source <(gz completion zsh)                          ~/.zshrc, after compinit
  fish        gz completion fish | source                          ~/.config/fish/config.fish
  powershell  gz completion powershell | Out-String | Invoke-Expression   $PROFILE
  elvish      eval (gz completion elvish | slurp)                  subcommands and flags only";

const BASH: &str = r#"_gz() {
    local IFS=$'\n'
    local line="${COMP_LINE:0:$COMP_POINT}"
    COMPREPLY=($(GZ_COMPLETE_LINE="$line" "${COMP_WORDS[0]}" __complete bash 2>/dev/null))
    local reply
    for reply in "${COMPREPLY[@]}"; do
        if [[ $reply == *. ]]; then
            compopt -o nospace 2>/dev/null
            break
        fi
    done
}
complete -o default -F _gz gz
"#;

const ZSH: &str = r#"#compdef gz
_gz() {
    local -a lines spaced spaced_shown joined joined_shown
    local line value about shown
    lines=("${(@f)$(GZ_COMPLETE_WORD="${words[CURRENT]}" "${words[1]}" __complete zsh "${(@)words[2,CURRENT-1]}" 2>/dev/null)}")
    for line in "${lines[@]}"; do
        [[ -z $line ]] && continue
        value=${line%%$'\t'*}
        about=${line#*$'\t'}
        shown=$value
        [[ $about != $line && -n $about ]] && shown="$value  -- $about"
        if [[ $value == *. ]]; then
            joined+=("$value"); joined_shown+=("$shown")
        else
            spaced+=("$value"); spaced_shown+=("$shown")
        fi
    done
    (( ${#spaced} )) && compadd -U -l -d spaced_shown -a spaced
    (( ${#joined} )) && compadd -U -l -S '' -d joined_shown -a joined
    return 0
}
compdef _gz gz
"#;

const FISH: &str = r#"function __gz_complete
    set -l before (commandline -opc)
    set -l current (commandline -ct)
    GZ_COMPLETE_WORD="$current" $before[1] __complete fish $before[2..-1] 2>/dev/null
end
complete -c gz -f -a '(__gz_complete)'
"#;

const POWERSHELL: &str = r#"Register-ArgumentCompleter -Native -CommandName gz -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    # The words wholly left of the cursor; the one under it is $wordToComplete.
    $before = @($commandAst.CommandElements |
        Where-Object { $_.Extent.EndOffset -lt $cursorPosition } |
        ForEach-Object { $_.Extent.Text })
    $program = $before[0]
    $env:GZ_COMPLETE_WORD = $wordToComplete
    try {
        $lines = & $program __complete powershell @($before | Select-Object -Skip 1) 2>$null
    } finally {
        Remove-Item Env:GZ_COMPLETE_WORD -ErrorAction SilentlyContinue
    }
    foreach ($line in $lines) {
        if (-not $line) { continue }
        $value, $about = $line -split "`t", 2
        if (-not $about) { $about = $value }
        $text = if ($value -match '[\s''"`$;,()]') { "'" + ($value -replace "'", "''") + "'" } else { $value }
        [System.Management.Automation.CompletionResult]::new($text, $value, 'ParameterValue', $about)
    }
}
"#;

/// Prints the completion script for `shell`.
pub fn print(shell: &str) -> Result<()> {
    let script = match shell.to_lowercase().as_str() {
        "bash" => BASH,
        "zsh" => ZSH,
        "fish" => FISH,
        "powershell" | "pwsh" => POWERSHELL,
        "elvish" => return elvish(),
        other => bail!("unsupported shell '{other}' (bash, zsh, fish, powershell, elvish)"),
    };
    print!("{script}");
    Ok(())
}

/// Elvish has no dynamic hook here; it gets clap's static script.
fn elvish() -> Result<()> {
    use clap::CommandFactory;
    let mut command = crate::cli::Cli::command();
    clap_complete::generate(
        clap_complete::Shell::Elvish,
        &mut command,
        "gz",
        &mut std::io::stdout(),
    );
    Ok(())
}
