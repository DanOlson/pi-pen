# Amazon Bedrock with AWS SSO

pi's Bedrock provider uses the AWS SDK, which reads `~/.aws/config` and the SSO
token cache. The container's home is the persistent home, so log in on the host
and copy those files into it. No image or code changes are needed.

> Not yet verified end to end against Bedrock. Test before relying on it.

## 1. Pin a namespace and AWS settings

In the project's `.pi-env.toml` (these values are not secrets):

```toml
[run]
namespace = "bedrock_demo"

[run.env]
AWS_PROFILE = "your-sso-profile"
AWS_REGION = "us-east-1"
```

## 2. Create the persistent home

Run `pi-sandbox run` once and quit, then find the home's path:

```sh
pi-sandbox namespace show
```

## 3. Log in on the host and copy the files in

```sh
aws sso login --profile your-sso-profile
H="<persistent home path from step 2>"
mkdir -p "$H/.aws/sso"
cp ~/.aws/config "$H/.aws/config"
cp -R ~/.aws/sso/cache "$H/.aws/sso/"
```

Do not copy `~/.aws/credentials`; it is not needed and may hold long-lived keys.

## 4. Smoke test

```sh
echo "Say hello" > /tmp/hello.txt
pi-sandbox exec --prompt-file /tmp/hello.txt --output-dir /tmp/pi-runs/bedrock-1 \
  -- --provider amazon-bedrock --model <bedrock model id>
```

Bedrock often needs an inference-profile ID (commonly `us.`-prefixed) rather
than the bare model ID.

## Notes

- **Expiry:** when the SSO session expires, repeat step 3. Profiles that use an
  `sso_session` block can refresh automatically until the SSO session ends. For
  a demo, log in and copy fresh that morning.
- **Spurious warning:** runs print "warning: no saved pi credentials…" on
  stderr. It is harmless: the check only looks at pi's `auth.json` and
  `*_API_KEY` variables.
- **Expired-token failures:** AWS reports expiry as `ExpiredTokenException`
  ("The security token included in the request is expired"), which the failure
  classifier may not recognize as an auth error. If a run fails that way, log
  in and copy again rather than retrying.
