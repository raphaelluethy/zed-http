((request
  (method) @run)
  (#set! tag http-request))

; `run #name` and `run ./file.http` parse as requests without a method.
((request
  url: (target_url) @run)
  (#match? @run "^run[ \t]")
  (#set! tag http-request))
