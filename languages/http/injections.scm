; Bodies
((json_body) @injection.content
  (#set! injection.language "json"))

((xml_body) @injection.content
  (#set! injection.language "xml"))

((graphql_data) @injection.content
  (#set! injection.language "graphql"))

; The grammar only recognizes JSON bodies that start with `{` or `[` and whitespace, so a
; one-line JSON body is a raw body.
((raw_body) @injection.content
  (#match? @injection.content "^[{\\[]")
  (#set! injection.language "json"))

; Pre-request scripts and response handlers are JavaScript.
((script) @injection.content
  (#set! injection.language "javascript"))
