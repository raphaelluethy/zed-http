; Request separators and their names
(request_separator) @comment
(request_separator
  value: (value) @title)

; Comments and `# @directive value` comments
(comment) @comment
(comment
  "@" @attribute
  name: (identifier) @attribute)
(comment
  value: (value) @string)

; Requests
(method) @function.method
; The grammar has no `run` or `import` lines; they parse as requests without a method.
((target_url) @string.url
  (#not-match? @string.url "^(run|import)[ \t]"))
((target_url) @keyword
  (#match? @keyword "^(run|import)[ \t]"))
(http_version) @keyword

; Headers
(header
  name: (header_entity) @property)
(header
  ":" @punctuation.delimiter)
(header
  value: (value) @string)

; File variables: `@name = value`
(variable_declaration
  "@" @operator
  name: (identifier) @variable)
(variable_declaration
  "=" @operator)
(variable_declaration
  value: (value) @string)

; `{{variable}}` references, wherever they appear
(variable) @variable.special
[
  "{{"
  "}}"
] @punctuation.bracket

; Scripts: `< {% %}` and `> {% %}`, or a script file
(pre_request_script
  "<" @operator)
(res_handler_script
  ">" @operator)
[
  "{%"
  "%}"
] @punctuation.special
(path) @string.special

; Response redirects: `>> file` and `>>! file`
(res_redirect
  path: (path) @string.special)

; Bodies loaded from files: `< ./body.json`
(external_body
  path: (_) @string.special)

; Responses
(status_code) @number
(status_text) @string
