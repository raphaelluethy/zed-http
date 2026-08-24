; Named request sections use their `### name` label.
(section
  (request_separator
    value: (value) @name)
  request: (request
    method: (method) @context
    url: (target_url) @context.extra)) @item

; Unnamed `###` sections fall back to the request URL.
(section
  (request_separator !value) @context
  request: (request
    method: (method) @context
    url: (target_url) @name)) @item

; Sections without a separator also use the request URL.
((section
  request: (request
    method: (method) @context
    url: (target_url) @name)) @item
  (#not-match? @item "^\\s*###"))
