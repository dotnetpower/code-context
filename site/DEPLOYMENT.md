# Website readiness and deployment

## Actual service and boundaries

The website is an Astro/Starlight static build with eight English documentation
pages, currently published at <https://dotnetpower.github.io/code-context/>.
It is not a commerce service, account issuer, or hosted code-search API.
There are no translated routes; unknown routes must return HTTP 404 with noindex.
No language alternates, ratings, payment protocols, or OAuth client provisioning
are advertised.

The only public data API is anonymous `GET /docs-index.json` under the site base:
it lists real rendered titles, summaries, canonical URLs, and Markdown mirrors.
The OpenAPI 3.1 description and RFC 9727 linkset describe only this endpoint.
Source indexing stays local; explicit HTTP probes contact their supplied URLs.
Published benchmark results describe specific fixtures, not guaranteed savings.

## Build and preflight

From the repository root:

```sh
npm --prefix site ci
npm --prefix site run check
npm --prefix site run build
npm --prefix site run check:seo
npm --prefix site test
cd site && npx playwright install chromium && npm run test:browser
```

The artifact gate enumerates every rendered indexable HTML file, rather than a
fixed route list. The Markdown corpus, JSON index, RSS and sitemaps all derive
from the same rendered pages, including the homepage, visible FAQs and caveats.
Sitemap shards contain at most 4,000 URLs. Dates are not fabricated on rebuild.
Search/retrieval bots are allowed; named training-only bots are opted out.
Root [robots.txt](public/robots.txt) is the source policy; the build renders the
same policy with the configured canonical sitemap URLs.

Node tests include negative contract mutations and alternating HTML/Markdown HTTP
requests. Chromium tests measure CLS session windows on every page at desktop
and mobile sizes, require CLS < 0.10, check static answers with JavaScript disabled,
and exercise the payload calculator. These are laboratory measurements, not
proof of field Core Web Vitals. Monitor production field data separately.

## HTTP hosting requirement

**GitHub Pages cannot set custom response headers or negotiate on Accept.**
Its project subpath also cannot publish domain-root robots.txt, IndexNow keys,
or `/.well-known/api-catalog`. A meta tag or a `_headers` file does not fix that.
Explicit Markdown mirrors work on Pages, but are not content negotiation.

The included [HTTP gateway](scripts/serve-agents.mjs) serves the same static
build, negotiates explicit Markdown preference using Accept quality values,
returns honest 404/405 statuses, and publishes discovery resources at both
the configured base path and domain root. The homepage sends RFC 8288 Link
headers. Both HTML and Markdown responses send:

```text
Vary: Accept, Accept-Encoding
Cache-Control: no-store
CDN-Cache-Control: no-store
```

Run `npm --prefix site run serve:agents` behind a TLS reverse proxy/CDN that
respects these headers, preserving the base path and forwarding the domain-root
discovery resources to the gateway. The default listener is `127.0.0.1:4321`;
`HOST` and `PORT` configure it. Never configure a missing-page SPA fallback.
The gateway does not expose local daemon APIs or workspace contents.

For an owned custom domain, set the **same** `SITE_URL` (origin) and `BASE_PATH`
(for example `/`) during build, server startup, and verification.
Do not change canonical domains before that domain actually serves this build.
Google verification is retained; `PUBLIC_GOOGLE_SITE_VERIFICATION` and
`PUBLIC_NAVER_SITE_VERIFICATION` accept real owner-issued verification values.
No Naver verification token is invented when none is configured.

## Post-deployment gate and search submissions

```sh
npm --prefix site run check:live -- https://your-owned-domain.example/
npm --prefix site run submit:indexing -- --dry-run
```

The live gate requires the exact built discovery files, both response
representations, no redirects on canonical pages, domain-root discovery and
noindex 404s. Cache isolation is checked in both alternating request orders.
A plain GitHub Pages deployment **fails** this strict gate by design.

The [Pages workflow](../.github/workflows/pages.yml) runs artifact, Node and
browser gates before deployment, then checks the exact deployed artifact.
Set repository variables `SITE_URL`, `BASE_PATH`, and `AGENT_SITE_URL` for the
actual gateway-backed canonical deployment. Until that infrastructure exists,
the post-deployment job remains red instead of claiming full agent readiness.
Deploying a separate gateway remains an operator responsibility; the workflow
does not pretend that uploading static Pages files starts a Node server.

IndexNow and GSC run only after the strict live gate succeeds. IndexNow verifies
the published key before POSTing batches of at most 10,000 canonical URLs.
GSC uses the authenticated Sitemaps API `PUT`, not the retired unauthenticated
ping endpoint. Configure `GSC_WORKLOAD_IDENTITY_PROVIDER` and `GSC_SERVICE_ACCOUNT`
for short-lived Google credentials with the `webmasters` scope, and grant the
service account access to the verified Search Console property. Alternatively,
provide a valid short-lived `GSC_ACCESS_TOKEN` repository secret.
`GSC_PROPERTY` optionally specifies a domain property such as `sc-domain:example.org`;
otherwise the canonical site-prefix property is used. Missing credentials or
provider rejection fail explicitly. Submission acceptance does not guarantee
indexing, rankings, answer-engine inclusion, or rich-result eligibility.
