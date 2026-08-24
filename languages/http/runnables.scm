; The `http-request` tag wires these to the runnable task in tasks.json.
(
  (request
    method: (method) @run) @request
  (#set! tag http-request)
)
