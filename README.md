# redscript-dap
Debug adapter server implementation for Cyberpunk 2077 scripts.

https://github.com/jac3km4/redscript-dap/assets/11986158/728a3de0-1d6d-47d4-8e9a-737a7a58a813

# requirements
- [redscript](https://github.com/jac3km4/redscript) 0.5.25 or newer
- [red4ext](https://github.com/WopsS/RED4ext) 1.25.0 or newer
- [redscript IDE VSCode extension](https://marketplace.visualstudio.com/items?itemName=jac3km4.redscript-ide-vscode) 0.2.6 or newer
- optionally RedMod DLC in Steam/GoG if you intend to debug built-in game scripts

# usage
- download the [latest DLL](https://github.com/jac3km4/redscript-dap/releases/latest) 
- place it in `{Cyberpunk Install Dir}/red4ext/plugins/redscript_dap.dll`
- start the game
- open a redscript workspace in VSCode, open the command pallette in VSCode (Ctrl+Shift+P) and execute `Redscript: Attach to Cyberpunk 2077`
- congratulations, you can now interact with the debugger, make sure to read about [known issues](#known-issues)

# known issues
- the debugger currently only stops at function calls, breakpoints set at lines that do not contain
  any function calls will do nothing, which may result in surprising jumps sometimes
    - intrinsics like `Equals`, `ArrayPush` and so on are not function calls and will not stop the
      debugger
- stepping might appear to be stuck on a line sometimes, but it's not - it's usually due to the
  fact that the line you're at contains a lot of function calls which might be non-obvious like
  casts and calls to operators, the debugger will sequentially step over them
- you cannot manually pause or access threads, use breakpoints instead
- the game currently doesn't give up focus on breakpoints, I recommend either running the
  game in windowed mode next to VSCode or running them on separate screens
- using 'Step Out' while in a function invoked directly from CET will cause the debugger to hang up
  and requires a restart
