((comment) @injection.content
  (#set! injection.language "comment"))

((json_body) @injection.content
  (#set! injection.language "json"))

((xml_body) @injection.content
  (#set! injection.language "xml"))

((graphql_data) @injection.content
  (#set! injection.language "graphql"))

((comment
  name: (_) @_name
  (#eq? @_name "lang")
  value: (_) @injection.language)?
  .
  (_
    (script) @injection.content
    (#offset! @injection.content 0 2 0 -2))
  (#set! injection.language "javascript"))
