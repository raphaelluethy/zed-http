; Requests and responses
(method) @function.method
(request url: (target_url) @string.special.url)
(http_version) @constant
(status_code) @number
(status_text) @string

; Headers
(header name: (header_entity) @property)
(header value: (value) @string)

; Variables and metadata
(variable name: (identifier) @variable)
(variable_declaration name: (identifier) @variable)
(variable_declaration "=" @operator)
(variable_declaration value: (value) @string)
(comment "@" @keyword name: (identifier) @keyword)
(comment "=" @operator)
(request_separator value: (value) @label)

; Bodies and file references
(raw_body) @string
(multipart_form_data) @string.special
(external_body path: (path) @string.special.path)
(pre_request_script (path) @string.special.path)
(res_handler_script (path) @string.special.path)
(res_redirect path: (path) @string.special.path)

; Punctuation
["{{" "}}"] @punctuation.bracket
(header ":" @punctuation.delimiter)

; Comments and request separators
[(comment) (request_separator)] @comment
