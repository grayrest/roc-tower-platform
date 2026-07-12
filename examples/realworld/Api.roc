import pf.Res

## RealWorld API conventions: the `{"errors":{"body":[...]}}` error shape,
## ISO-8601 timestamps, and slug generation. Pure helpers only.
Api := [].{
    ## A field-keyed RealWorld error response: `{"errors":{"<field>":["<message>"]}}`
    ## (the shape the official Hurl conformance suite asserts on, e.g.
    ## `errors.username[0] == "can't be blank"`). The key is dynamic, so the
    ## body is interpolated rather than encoder-derived; field/message are
    ## app constants, never user input. Unannotated (like `Req.reject`) so
    ## the empty effects list unifies with the app's effectful Response; the
    ## status is pinned to U16 explicitly because top-level values can't
    ## carry method-constrained numerals.
    field_error = |status, field, message| {
        code : U16
        code = status
        {
            status: code,
            headers: [{ name: "content-type", value: "application/json" }],
            body: "{\"errors\":{\"${field}\":[\"${message}\"]}}".to_utf8(),
            effects: [],
        }
    }

    ## The same field-keyed shape as an effect-free Reply — what a `Must`
    ## effect's err_handler returns (`Res.Reply` has no effects field).
    reply = |status, field, message| {
        code : U16
        code = status
        {
            status: code,
            headers: [{ name: "content-type", value: "application/json" }],
            body: "{\"errors\":{\"${field}\":[\"${message}\"]}}".to_utf8(),
        }
    }

    ## Generic 4xx/5xx keyed under "body" (malformed payloads, db errors).
    error = |status, message| Api.field_error(status, "body", message)

    ## 422 for a blank required field: `errors.<field> = ["can't be blank"]`.
    blank = |field| Api.field_error(422, field, "can't be blank")

    ## Validate a required field is non-blank (whitespace counts as blank).
    nonblank = |field, value| {
        if value.trim().is_empty() {
            Err(Api.blank(field))
        } else {
            Ok(value)
        }
    }

    ## 401 Unauthorized — no/invalid token: `errors.token = ["is missing"]`.
    unauthorized = || Api.field_error(401, "token", "is missing")

    ## 401 for wrong login: `errors.credentials = ["invalid"]`, deliberately
    ## not saying which of email/password was wrong.
    invalid_credentials = || Api.field_error(401, "credentials", "invalid")

    ## 403 Forbidden — authenticated but not the owner of `what`.
    forbidden = |what| Api.field_error(403, what, "forbidden")

    ## 404 Not Found: `errors.<what> = ["not found"]`.
    not_found = |what| Api.field_error(404, what, "not found")

    ## Milliseconds since the Unix epoch as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
    ## Civil-from-days per Howard Hinnant's algorithm; valid for all
    ## post-1970 instants, which is all `Req.now!` can produce.
    iso8601 : U64 -> Str
    iso8601 = |ms| {
        millis = ms % 1000
        total_seconds = ms // 1000
        second = total_seconds % 60
        minute = (total_seconds // 60) % 60
        hour = (total_seconds // 3600) % 24
        days = total_seconds // 86400

        z = days + 719468
        era = z // 146097
        doe = z - era * 146097
        yoe = (doe - doe // 1460 + doe // 36524 - doe // 146096) // 365
        doy = doe - (365 * yoe + yoe // 4 - yoe // 100)
        mp = (5 * doy + 2) // 153
        day = doy - (153 * mp + 2) // 5 + 1
        month = if mp < 10 mp + 3 else mp - 9
        year_of_era = yoe + era * 400
        year = if month <= 2 year_of_era + 1 else year_of_era

        "${year.to_str()}-${pad2(month)}-${pad2(day)}T${pad2(hour)}:${pad2(minute)}:${pad2(second)}.${pad3(millis)}Z"
    }



    ## URL slug from a title plus the article's reserved id, which makes the
    ## slug unique without probing the DB: lowercased ASCII alphanumerics,
    ## everything else collapsed to single dashes.
    ##
    ##     slugify("How to Train Your Dragon", 7) == "how-to-train-your-dragon-7"
    slugify : Str, I64 -> Str
    slugify = |title, id| {
        folded = title.to_utf8().fold({ out: [], dash_pending : False }, |acc, byte| {
            lower = if byte >= 65 and byte <= 90 byte + 32 else byte
            is_alnum = (lower >= 97 and lower <= 122) or (lower >= 48 and lower <= 57)
            if is_alnum {
                out = if acc.dash_pending and !acc.out.is_empty() acc.out.append(45) else acc.out
                { out: out.append(lower), dash_pending: False }
            } else {
                { out: acc.out, dash_pending: True }
            }
        })
        stem = Str.from_utf8_lossy(folded.out)
        if stem.is_empty() {
            "article-${id.to_str()}"
        } else {
            "${stem}-${id.to_str()}"
        }
    }
}

# --- module-private helpers (unreachable from other modules) ---

## Internal: zero-pad to two digits.
pad2 : U64 -> Str
pad2 = |n| if n < 10 "0${n.to_str()}" else n.to_str()

## Internal: zero-pad to three digits.
pad3 : U64 -> Str
pad3 = |n| {
    if n < 10 {
        "00${n.to_str()}"
    } else if n < 100 {
        "0${n.to_str()}"
    } else {
        n.to_str()
    }
}

## the epoch itself formats as 1970-01-01T00:00:00.000Z
expect Api.iso8601(0) == "1970-01-01T00:00:00.000Z"

## a known instant with sub-second millis (10^12 ms)
expect Api.iso8601(1000000000000) == "2001-09-09T01:46:40.000Z"

## a modern new-year boundary formats correctly
expect Api.iso8601(1735689600000) == "2025-01-01T00:00:00.000Z"

## leap-day handling: 2024-02-29
expect Api.iso8601(1709164800000) == "2024-02-29T00:00:00.000Z"

## millis are zero-padded to three digits
expect Api.iso8601(7) == "1970-01-01T00:00:00.007Z"

## slugify lowercases, collapses punctuation runs, and appends the id
expect Api.slugify("How to Train Your Dragon", 7) == "how-to-train-your-dragon-7"

## slugify collapses consecutive separators into one dash
expect Api.slugify("Hello --- World!!", 3) == "hello-world-3"

## slugify of an all-punctuation title falls back to article-{id}
expect Api.slugify("!!!", 9) == "article-9"

## field_error produces the field-keyed RealWorld error shape
expect {
    response = Api.field_error(409, "email", "has already been taken")
    response.status == 409 and Str.from_utf8_lossy(response.body) == "{\"errors\":{\"email\":[\"has already been taken\"]}}"
}

## nonblank rejects whitespace-only values with the can't-be-blank shape
expect {
    match Api.nonblank("username", "  ") {
        Ok(_) => False
        Err(response) => response.status == 422 and Str.from_utf8_lossy(response.body) == "{\"errors\":{\"username\":[\"can't be blank\"]}}"
    }
}

## nonblank passes real values through
expect Api.nonblank("username", "jake") == Ok("jake")

## not_found keys the error under the missing resource
expect {
    response = Api.not_found("article")
    response.status == 404 and Str.from_utf8_lossy(response.body) == "{\"errors\":{\"article\":[\"not found\"]}}"
}
