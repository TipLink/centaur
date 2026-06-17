require "net/http"
require "json"
require "uri"
require "base64"
require "openssl"
require "time"

module Broker
  # GithubAppClient mints a GitHub App *installation* access token. GitHub App
  # tokens are not an RFC 6749 grant: sign a short-lived app JWT (RS256, signed
  # with the App private key), present it as a Bearer credential to
  # POST /app/installations/{id}/access_tokens, and read back {token, expires_at}.
  # It returns the same Result shape as RefreshClient (refresh_token is always
  # nil -- there is nothing to rotate; the next mint re-signs a fresh JWT), so
  # BrokerCredential#apply_success! handles both paths identically. Ported from
  # the original services/api github_app_token_broker.py shim.
  #
  # SECURITY: this class never logs the private key, the signed JWT, the minted
  # access token, or the response body. Callers must keep the same discipline.
  class GithubAppClient
    # Reuse the broker Result contract so apply_success! is grant-agnostic.
    Result = Broker::RefreshClient::Result
    # The minimal HTTP response shape, so tests can inject a double.
    Response = Broker::RefreshClient::Response

    DEFAULT_TIMEOUT = 30
    MAX_BODY_BYTES = 64 * 1024
    # The app JWT lives only long enough to exchange it. 60s backdating absorbs
    # minor clock skew; GitHub rejects an `exp` more than 10 minutes out.
    JWT_BACKDATE_SECONDS = 60
    JWT_LIFETIME_SECONDS = 9 * 60
    ACCEPT = "application/vnd.github+json".freeze
    API_VERSION = "2022-11-28".freeze

    # http: an optional callable for testing, invoked as
    #   http.call(url:, headers:, timeout:) -> Response
    # When nil, a Net::HTTP-backed implementation is used.
    def initialize(http: nil)
      @http = http
    end

    # Mints one installation token. Raises Broker::RefreshError on failure
    # (classified retryable vs. unrecoverable), matching RefreshClient so the
    # credential's backoff/dead state machine is identical for both kinds.
    #   token_endpoint: full https://api.github.com/app/installations/{id}/access_tokens
    #   app_id:         the GitHub App id, used as the JWT issuer
    #   private_key:    the App private key PEM (escaped-newline form tolerated)
    def mint(token_endpoint:, app_id:, private_key:, timeout: DEFAULT_TIMEOUT)
      raise ArgumentError, "token endpoint is required" if token_endpoint.blank?
      raise ArgumentError, "app_id is required" if app_id.blank?
      raise ArgumentError, "private_key is required" if private_key.blank?

      jwt = app_jwt(app_id, private_key)
      headers = {
        "Authorization" => "Bearer #{jwt}",
        "Accept" => ACCEPT,
        "X-GitHub-Api-Version" => API_VERSION
      }
      response = perform(token_endpoint, headers, timeout)

      return classify_error(response.status, response.body) if response.status / 100 != 2

      parse_success(response)
    end

    private

    # Builds and signs the app JWT with the stdlib OpenSSL RSA signer (the `jwt`
    # gem is intentionally not a dependency). Mirrors the escaped-newline
    # handling the original shim used for env-stored PEMs.
    def app_jwt(app_id, private_key)
      now = Time.now.to_i
      header = { alg: "RS256", typ: "JWT" }
      payload = {
        iat: now - JWT_BACKDATE_SECONDS,
        exp: now + JWT_LIFETIME_SECONDS,
        iss: app_id.to_s
      }
      signing_input = "#{b64url(JSON.generate(header))}.#{b64url(JSON.generate(payload))}"
      key = OpenSSL::PKey::RSA.new(normalize_pem(private_key))
      signature = key.sign(OpenSSL::Digest.new("SHA256"), signing_input)
      "#{signing_input}.#{b64url(signature)}"
    rescue OpenSSL::PKey::RSAError, OpenSSL::PKey::PKeyError
      # A malformed/garbled private key is a configuration error, not a transient
      # fault: surface it as unrecoverable so the credential goes dead and a human
      # re-supplies the key rather than spinning in backoff forever.
      raise RefreshError.new("github app private key is invalid",
                             stage: "oauth", code: "invalid_private_key", retryable: false)
    end

    def normalize_pem(key)
      key = key.to_s.strip
      key = key.gsub("\\n", "\n") if key.include?("\\n") && !key.include?("\n")
      key
    end

    def b64url(bytes)
      Base64.urlsafe_encode64(bytes, padding: false)
    end

    def perform(url, headers, timeout)
      if @http
        return @http.call(url: url, headers: headers, timeout: timeout)
      end

      uri = URI.parse(url)
      req = Net::HTTP::Post.new(uri)
      headers.each { |name, value| req[name] = value }

      http = Net::HTTP.new(uri.host, uri.port)
      http.use_ssl = uri.scheme == "https"
      http.open_timeout = timeout
      http.read_timeout = timeout

      res = http.request(req)
      Response.new(status: res.code.to_i, body: res.body.to_s.byteslice(0, MAX_BODY_BYTES))
    rescue StandardError => e
      # Network/transport failures are transient: a brief outage must not mark the
      # credential dead. Backoff exhaustion is the louder signal.
      raise RefreshError.new("github token endpoint request failed: #{e.class}",
                             stage: "network", retryable: true)
    end

    def parse_success(response)
      parsed = JSON.parse(response.body)
      token = parsed["token"]
      if token.blank?
        raise RefreshError.new("github token endpoint returned an empty token",
                               stage: "parse", status: response.status, retryable: true)
      end

      expires_in = expires_in_from(parsed["expires_at"])
      Result.new(access_token: token, refresh_token: nil, expires_in: expires_in)
    rescue JSON::ParserError, ArgumentError, TypeError
      raise RefreshError.new("parsing github token response failed",
                             stage: "parse", status: response.status, retryable: true)
    end

    # GitHub returns an absolute ISO-8601 expires_at; convert to the seconds-from-
    # now expires_in that the Result contract carries. nil/unparseable lets the
    # caller fall back to its conservative default.
    def expires_in_from(expires_at)
      return nil if expires_at.blank?
      seconds = (Time.iso8601(expires_at.to_s) - Time.now).to_i
      seconds.positive? ? seconds : nil
    rescue ArgumentError
      nil
    end

    # GitHub's auth failures on this endpoint (401 bad JWT/clock, 404 wrong
    # installation, 422) are structural -- the credential is dead until a human
    # fixes the App config. 5xx and bodyless/transport 4xx are retryable.
    def classify_error(status, body)
      github_error = begin
        JSON.parse(body.to_s)["message"]
      rescue JSON::ParserError, TypeError
        nil
      end

      if status / 100 == 5
        raise RefreshError.new("github token endpoint http #{status}",
                               stage: "http", status: status, retryable: true)
      end

      if [ 401, 403, 404, 422 ].include?(status)
        raise RefreshError.new("github token endpoint rejected app credential (#{status})",
                               stage: "oauth", code: github_error.presence || "github_#{status}",
                               status: status, retryable: false)
      end

      raise RefreshError.new("github token endpoint http #{status}",
                             stage: "http", status: status, retryable: true)
    end
  end
end
