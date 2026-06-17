require "base64"
require "json"
require "net/http"
require "openssl"
require "time"
require "uri"

module Broker
  # Mints GitHub App installation tokens. Unlike OAuth refresh-token credentials,
  # GitHub App installation tokens have no refresh token; each renewal signs a
  # short-lived App JWT and exchanges it for a new installation token.
  class GithubAppInstallationClient
    Response = Data.define(:status, :body)

    DEFAULT_TIMEOUT = 30
    MAX_BODY_BYTES = 64 * 1024

    def initialize(http: nil)
      @http = http
    end

    def mint(token_endpoint:, app_id:, private_key_pem:, timeout: DEFAULT_TIMEOUT, now: Time.current)
      raise ArgumentError, "token endpoint is required" if token_endpoint.blank?
      raise ArgumentError, "app_id is required" if app_id.blank?
      raise ArgumentError, "private_key_pem is required" if private_key_pem.blank?

      jwt = app_jwt(app_id: app_id, private_key_pem: private_key_pem, now: now)
      response = perform(token_endpoint, jwt, timeout)

      return classify_error(response.status, response.body) if response.status / 100 != 2

      parse_success(response, now: now)
    end

    private

    def app_jwt(app_id:, private_key_pem:, now:)
      payload = {
        iat: now.to_i - 60,
        exp: now.to_i + 9.minutes.to_i,
        iss: app_id
      }
      signing_input = [
        base64url({ alg: "RS256", typ: "JWT" }.to_json),
        base64url(payload.to_json)
      ].join(".")
      key = OpenSSL::PKey::RSA.new(normalize_pem(private_key_pem))
      signature = key.sign(OpenSSL::Digest.new("SHA256"), signing_input)
      "#{signing_input}.#{base64url(signature)}"
    rescue OpenSSL::PKey::PKeyError, OpenSSL::PKey::RSAError, ArgumentError
      raise RefreshError.new("invalid GitHub App private key",
                             stage: "config", code: "invalid_private_key", retryable: false)
    end

    # GitHub App private keys are often stored env-escaped (literal "\n" rather
    # than real newlines) -- the fineas-github-app secret is one such source (the
    # token-refresher CronJob expands it with `printf '%b'`). OpenSSL needs real
    # newlines, so expand them when the PEM has no real newline of its own.
    def normalize_pem(pem)
      pem = pem.to_s.strip
      pem = pem.gsub("\\n", "\n") if pem.include?("\\n") && !pem.include?("\n")
      pem
    end

    def base64url(value)
      Base64.urlsafe_encode64(value, padding: false)
    end

    def perform(url, jwt, timeout)
      if @http
        return @http.call(url: url, headers: github_headers(jwt), timeout: timeout)
      end

      uri = URI.parse(url)
      req = Net::HTTP::Post.new(uri)
      github_headers(jwt).each { |name, value| req[name] = value }

      http = Net::HTTP.new(uri.host, uri.port)
      http.use_ssl = uri.scheme == "https"
      http.open_timeout = timeout
      http.read_timeout = timeout

      res = http.request(req)
      Response.new(status: res.code.to_i, body: res.body.to_s.byteslice(0, MAX_BODY_BYTES))
    rescue StandardError => e
      raise RefreshError.new("GitHub App token request failed: #{e.class}",
                             stage: "network", retryable: true)
    end

    def github_headers(jwt)
      {
        "Accept" => "application/vnd.github+json",
        "Authorization" => "Bearer #{jwt}",
        "X-GitHub-Api-Version" => "2022-11-28"
      }
    end

    def parse_success(response, now:)
      parsed = JSON.parse(response.body)
      access_token = parsed["token"]
      if access_token.blank?
        raise RefreshError.new("GitHub returned an empty installation token",
                               stage: "parse", status: response.status, retryable: true)
      end
      expires_at = Time.iso8601(parsed.fetch("expires_at"))
      expires_in = [ (expires_at - now).to_i, 1 ].max
      RefreshClient::Result.new(access_token: access_token, refresh_token: nil, expires_in: expires_in)
    rescue JSON::ParserError, KeyError, ArgumentError
      raise RefreshError.new("parsing GitHub App token response failed",
                             stage: "parse", status: response.status, retryable: true)
    end

    def classify_error(status, body)
      message = begin
        JSON.parse(body.to_s)["message"]
      rescue JSON::ParserError, TypeError
        nil
      end

      if status / 100 == 5 || status == 429 || message.blank?
        raise RefreshError.new("GitHub App token endpoint http #{status}",
                               stage: "http", code: message.presence, status: status, retryable: true)
      end

      raise RefreshError.new("GitHub App token endpoint rejected credential: #{message}",
                             stage: "github", code: message, status: status, retryable: false)
    end
  end
end
