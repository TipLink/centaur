require "test_helper"

module Broker
  class GithubAppClientTest < ActiveSupport::TestCase
    # A stub HTTP backend matching GithubAppClient's injected contract. Captures
    # the request so tests can assert the headers without a real socket.
    class StubHTTP
      attr_reader :captured

      def initialize(status:, body:)
        @status = status
        @body = body
      end

      def call(url:, headers:, timeout:)
        @captured = { url: url, headers: headers, timeout: timeout }
        Broker::GithubAppClient::Response.new(status: @status, body: @body)
      end
    end

    # One 2048-bit RSA key for the whole case: generation is the slow part, and
    # the signature only has to verify against itself.
    PRIVATE_KEY = OpenSSL::PKey::RSA.generate(2048).freeze

    def client_with(status:, body:)
      http = StubHTTP.new(status: status, body: body)
      [ Broker::GithubAppClient.new(http: http), http ]
    end

    def base_args(**overrides)
      {
        token_endpoint: "https://api.github.com/app/installations/42/access_tokens",
        app_id: "123456",
        private_key: PRIVATE_KEY.to_pem
      }.merge(overrides)
    end

    test "successful mint parses {token, expires_at} into the Result contract" do
      expires_at = (Time.now + 3600).utc.iso8601
      client, _ = client_with(status: 201, body: { token: "ghs_minted", expires_at: expires_at }.to_json)
      result = client.mint(**base_args)
      assert_equal "ghs_minted", result.access_token
      assert_nil result.refresh_token
      assert_in_delta 3600, result.expires_in, 60
    end

    test "presents a verifiable RS256 app JWT as the Bearer credential" do
      client, http = client_with(status: 201, body: { token: "ghs_x", expires_at: (Time.now + 60).utc.iso8601 }.to_json)
      client.mint(**base_args)

      auth = http.captured[:headers]["Authorization"]
      assert_match(/\ABearer ey/, auth)
      assert_equal "application/vnd.github+json", http.captured[:headers]["Accept"]
      assert_equal "2022-11-28", http.captured[:headers]["X-GitHub-Api-Version"]

      jwt = auth.delete_prefix("Bearer ")
      header_b64, payload_b64, signature_b64 = jwt.split(".")
      signing_input = "#{header_b64}.#{payload_b64}"
      signature = Base64.urlsafe_decode64(signature_b64)
      assert PRIVATE_KEY.verify(OpenSSL::Digest.new("SHA256"), signature, signing_input),
             "JWT signature must verify against the app private key"

      payload = JSON.parse(Base64.urlsafe_decode64(payload_b64 + "=" * ((4 - payload_b64.length % 4) % 4)))
      assert_equal "123456", payload["iss"]
      assert payload["exp"] > payload["iat"]
    end

    test "accepts an escaped-newline PEM" do
      escaped = PRIVATE_KEY.to_pem.gsub("\n", "\\n")
      client, _ = client_with(status: 201, body: { token: "ghs_x", expires_at: (Time.now + 60).utc.iso8601 }.to_json)
      assert_equal "ghs_x", client.mint(**base_args(private_key: escaped)).access_token
    end

    test "a malformed private key is unrecoverable" do
      client, _ = client_with(status: 201, body: "{}")
      err = assert_raises(Broker::RefreshError) { client.mint(**base_args(private_key: "-----BEGIN RSA PRIVATE KEY-----\nnope\n-----END RSA PRIVATE KEY-----")) }
      refute err.retryable?
      assert_equal "invalid_private_key", err.code
    end

    test "401/404 from GitHub are unrecoverable (app/installation misconfig)" do
      [ 401, 404, 422 ].each do |status|
        client, _ = client_with(status: status, body: { message: "Bad credentials" }.to_json)
        err = assert_raises(Broker::RefreshError) { client.mint(**base_args) }
        refute err.retryable?, "status #{status} must be unrecoverable"
      end
    end

    test "5xx is retryable" do
      client, _ = client_with(status: 502, body: "bad gateway")
      err = assert_raises(Broker::RefreshError) { client.mint(**base_args) }
      assert err.retryable?
    end

    test "missing token in 2xx is retryable" do
      client, _ = client_with(status: 201, body: { expires_at: (Time.now + 60).utc.iso8601 }.to_json)
      err = assert_raises(Broker::RefreshError) { client.mint(**base_args) }
      assert err.retryable?
      assert_equal "parse", err.stage
    end

    test "malformed 2xx body is a retryable parse failure" do
      client, _ = client_with(status: 201, body: "not json{")
      err = assert_raises(Broker::RefreshError) { client.mint(**base_args) }
      assert err.retryable?
      assert_equal "parse", err.stage
    end

    test "validates required inputs" do
      client, _ = client_with(status: 201, body: "{}")
      assert_raises(ArgumentError) { client.mint(**base_args(app_id: "")) }
      assert_raises(ArgumentError) { client.mint(**base_args(private_key: "")) }
      assert_raises(ArgumentError) { client.mint(**base_args(token_endpoint: "")) }
    end
  end
end
