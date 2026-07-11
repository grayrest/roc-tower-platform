app [handle!] { pf: platform "../../platform/main.roc" }

handle! : Str => Str
handle! = |path| "Hello from Roc! You requested: ${path}"
