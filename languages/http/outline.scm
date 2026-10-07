; Named sections contain their request and its headers in Zed's outline.
(section
  (request_separator value: (value) @name)
  request: (request)) @item

(request
  method: (method)? @context
  url: (target_url) @name) @item

(header name: (header_entity) @name) @item
