require "test_helper"
require "base64"

module Broker
  class GithubAppInstallationClientTest < ActiveSupport::TestCase
    class StubHTTP
      attr_reader :captured

      def initialize(status:, body:)
        @status = status
        @body = body
      end

      def call(url:, headers:, timeout:)
        @captured = { url: url, headers: headers, timeout: timeout }
        GithubAppInstallationClient::Response.new(status: @status, body: @body)
      end
    end

    def private_key = OpenSSL::PKey::RSA.generate(2048).to_pem

    def client_with(status:, body:)
      http = StubHTTP.new(status: status, body: body)
      [ GithubAppInstallationClient.new(http: http), http ]
    end

    def base_args(**overrides)
      {
        token_endpoint: "https://api.github.com/app/installations/42/access_tokens",
        app_id: "12345",
        private_key_pem: private_key,
        now: Time.zone.parse("2026-06-17T12:00:00Z")
      }.merge(overrides)
    end

    test "mints a GitHub App installation token using an app JWT" do
      expires_at = "2026-06-17T13:00:00Z"
      client, http = client_with(status: 201, body: { token: "ghs_installation", expires_at: expires_at }.to_json)

      result = client.mint(**base_args(timeout: 12))

      assert_equal "ghs_installation", result.access_token
      assert_nil result.refresh_token
      assert_equal 3600, result.expires_in
      assert_equal "https://api.github.com/app/installations/42/access_tokens", http.captured[:url]
      assert_equal "application/vnd.github+json", http.captured[:headers]["Accept"]
      assert_equal "2022-11-28", http.captured[:headers]["X-GitHub-Api-Version"]
      assert_equal 12, http.captured[:timeout]

      auth = http.captured[:headers].fetch("Authorization")
      assert_match(/\ABearer /, auth)
      header, payload, signature = auth.delete_prefix("Bearer ").split(".")
      assert JSON.parse(Base64.urlsafe_decode64(header)).slice("alg", "typ") == { "alg" => "RS256", "typ" => "JWT" }
      claims = JSON.parse(Base64.urlsafe_decode64(payload))
      assert_equal "12345", claims["iss"]
      assert claims["exp"] > claims["iat"]
      assert signature.present?
    end

    test "invalid private key is unrecoverable" do
      client, = client_with(status: 201, body: "{}")
      err = assert_raises(RefreshError) do
        client.mint(**base_args(private_key_pem: "not a key"))
      end
      refute err.retryable?
      assert_equal "invalid_private_key", err.code
    end

    test "GitHub 5xx is retryable" do
      client, = client_with(status: 502, body: "bad gateway")
      err = assert_raises(RefreshError) { client.mint(**base_args) }
      assert err.retryable?
    end

    test "GitHub credential rejection is unrecoverable" do
      client, = client_with(status: 401, body: { message: "Bad credentials" }.to_json)
      err = assert_raises(RefreshError) { client.mint(**base_args) }
      refute err.retryable?
      assert_equal "Bad credentials", err.code
    end
  end
end
