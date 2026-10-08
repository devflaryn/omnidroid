# omnidroid plugins

The launcher (`omnidroid`) is the same for every app. What only one app, one test, or one person
needs goes in a plugin: options of its own, environment variables, default arguments, programs run
around a session, and commands. A plugin is loaded only for someone who installed it, so nothing
here runs for anyone who did not ask for it.

```text
omnidroid plugins                         # list what is installed
omnidroid plugins add --link plugins/mytest   # install it where it is (edits are live)
omnidroid plugins add plugins/mytest      # or install a copy
omnidroid plugins disable mytest          # switch it off (enable to switch it on again)
omnidroid plugins remove mytest           # uninstall (a linked directory is left alone)
omnidroid plugins new mytest              # make ./mytest to start from
omnidroid help                            # the launcher's options, then every installed plugin's
```

Plugins are installed in `<app-data>/../plugins` (macOS: `~/Library/Application Support/Omnidroid/plugins`;
Windows: `%LOCALAPPDATA%\Omnidroid\plugins`; Linux: `$XDG_DATA_HOME/omnidroid/plugins`), or in
`OMNI_PLUGINS` when set.

## Here

This repository ships no plugins yet. `omnidroid plugins new <name>` makes a starting point.

## Writing one

A plugin is a directory with an `omnidroid-plugin.json`. Every field except `name` is optional:

```json
{
  "name": "mytest",
  "version": "0.1.0",
  "description": "What this plugin is for",

  "args": [
    { "flag": "--level", "type": "integer", "value": "n", "min": 1, "max": 9,
      "commands": ["play", "aosp"], "env": "OMNI_MYTEST_LEVEL",
      "help": "start at this level" },
    { "flag": "--verbose-log", "type": "switch", "commands": ["play"],
      "env": { "play": "OMNI_MYTEST_LOG" } }
  ],

  "env":      { "*": { "OMNI_MYTEST": "1" }, "aosp": { "OMNI_GPU": "gl" } },
  "defaults": { "play": ["--phone"] },
  "hooks":    { "session-start": ["python3", "${PLUGIN_DIR}/start.py"],
                "session-end":   ["python3", "${PLUGIN_DIR}/end.py"] },
  "commands": { "mytest-reset": { "run": ["python3", "${PLUGIN_DIR}/reset.py"],
                                  "help": "clear the app's saved data before a test" } }
}
```

* **`args`**: options for `play` and/or `aosp` (`commands`, default both). `type` is `switch` (no
  value), `string` (the default), `path` (made absolute), `integer` or `number` (with `min`/`max`).
  The launcher checks the value and takes the option out before reading its own. With `env`, the
  value goes to the session in that variable (`"VAR"` for every command, or `{ "play": "VAR" }`);
  a switch gives `1`. A hook always sees it as `OMNI_ARG_<NAME>` (`--verbose-log` is
  `OMNI_ARG_VERBOSE_LOG`). An option the launcher already has, or another installed plugin adds, is
  refused.
* **`env`**: variables every session of a command gets (`"*"`: every command). They are applied
  after the launcher's own, so a plugin can change the launcher's behaviour (`OMNI_GPU`,
  `OMNI_KEYBOARD_MOUSE`, ...).
* **`defaults`**: arguments put in front of the person's own; theirs come later and win.
* **`hooks`**: programs run at `session-start` (the APK is chosen, the session has not started) and
  `session-end`. A `session-start` hook answers on stdout, one line each:
  * `env KEY=VALUE`: a variable for the session (never printed by the launcher)
  * `unset KEY`: a variable the session must not have
  * `data-dir <dir>`: `play`'s storage directory, unless the person gave `--data-dir`
  * `say <text>`: shown as `Omnidroid [<plugin>]: <text>`
  * `error <text>`: the session does not start

  Other lines are shown as they are; stderr goes to the terminal; a hook that exits non-zero stops
  the session. `session-end` is also given `OMNI_EXIT_CODE`.
* **`commands`**: `omnidroid <name> [args...]` runs `run` with the arguments after the name.

Every program a plugin runs gets `OMNI_PLUGIN_NAME`, `OMNI_PLUGIN_DIR`, `OMNI_REPO`, `OMNI_COMMAND`
and `OMNI_APP_DATA_DIR`; a session's hooks also get `OMNI_FRESH` (`1`/`0`; `--fresh`, or
`--fresh-device` for `aosp`), `OMNI_GIVEN_DATA_DIR`, `OMNI_APK` and `OMNI_APK_PACKAGE`.
`${PLUGIN_DIR}`, `${REPO}`, `${APP_DATA_DIR}` and `${CARGO}` in `run`, hook and `env` values are
replaced with theirs.

A plugin that is broken (its manifest does not parse, or it clashes with another) stops the launcher
with what is wrong and how to switch it off: `omnidroid plugins disable <name>`.

The launcher's side is `crates/omnidroid/src/plugin.rs`.
