# Google auth fixture

`service-account.json` is a **throwaway** service-account key generated with
`openssl genrsa 2048` for the Rust test suite. It has never been registered with
Google and authorizes nothing. It exists so `ServiceAccountTokenSource` can sign a
real RS256 JWT in tests and have that JWT decoded and inspected.

Do not replace it with a real key.
