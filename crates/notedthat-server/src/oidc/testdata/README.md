# Test-only key material

`rs256-private.pem` and the matching `jwks.json` (`kid: test-2026`) sign and
verify the tokens the OIDC tests mint. They were generated once with

```sh
openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out rs256-private.pem
```

and are **test fixtures only**: never configure a deployment with them.
