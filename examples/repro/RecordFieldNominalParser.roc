app [main!] {
	pf: platform "https://github.com/roc-lang/basic-cli/releases/download/0.21.0/4rAQg8kUYZ3Vksr4qMQHpaFYNiHSn9GgS7gVxghd1XYV.tar.zst",
}

import pf.OsStr exposing [OsStr]
import pf.Stdout

## A nominal that supplies its own `parser_for`.
Nullable := [Null, Val(Str)].{
	parser_for = |encoding| |state| match Json.parse_null(encoding, state) {
		Ok(rest) => Ok({ value: Nullable.Null, rest: rest })
		Err(_) => match Json.parse_str(encoding, state) {
			Ok({ value, rest }) => Ok({ value: Nullable.Val(value), rest: rest })
			Err(problem) => Err(problem)
		}
	}
}

main! : List(OsStr) => Try({}, _)
main! = |_args| {
	## Deriving a RECORD parser whose field is that nominal is the trigger.
	body : { bio : Nullable }
	body = Json.parse("{\"bio\":null}") ? |_| Failed

	bio = body.bio
	txt = match bio {
		Nullable.Val(s) => s
		Nullable.Null => "null"
	}

	Stdout.line!(txt)?
	Ok({})
}
