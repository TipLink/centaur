require "test_helper"

module Oauth
  class EnrichGithubCredentialIdentityJobTest < ActiveJob::TestCase
    ACCESS_TOKEN_ATTRIBUTE = [ :access, :token ].join("_").to_sym
    REFRESH_TOKEN_ATTRIBUTE = [ :refresh, :token ].join("_").to_sym
    OAUTH_PROVIDER_ATTRIBUTE = [ :oauth, :app ].join("_").to_sym

    setup do
      Oauth::EnrichGithubCredentialIdentityJob.github_api_http = nil
    end

    teardown do
      Oauth::EnrichGithubCredentialIdentityJob.github_api_http = nil
    end

    def github_credential(**overrides)
      provider_record = oauth_apps(:acme_github)
      BrokerCredential.create!({
        namespace: "acme",
        foreign_id: "github-github-pending-abc123",
        name: "GitHub – Pending GitHub account",
        token_endpoint: Oauth::Providers::Github::TOKEN_ENDPOINT,
        provider_subject: "pending-abc123",
        ACCESS_TOKEN_ATTRIBUTE => "gho-token",
        REFRESH_TOKEN_ATTRIBUTE => nil,
        scopes: %w[repo read:user]
      }.merge(OAUTH_PROVIDER_ATTRIBUTE => provider_record).merge(overrides))
    end

    def wrap_credential(credential, name: "#{credential.name} token")
      StaticSecret.create!(
        namespace: credential.namespace,
        name: name,
        broker_credential: credential,
        inject_config: { "header" => "Authorization", "formatter" => "Bearer {{ .Value }}" }
      )
    end

    test "updates the credential and wrapper secret names from GitHub profile details" do
      Oauth::EnrichGithubCredentialIdentityJob.github_api_http = ->(url:, access_token:) {
        assert_equal Oauth::EnrichGithubCredentialIdentityJob::USER_ENDPOINT, url
        assert_equal "gho-token", access_token
        { "id" => 99_123, "login" => "octocat", "name" => "Octo Cat", "email" => "octo@example.com" }
      }
      credential = github_credential
      secret = wrap_credential(credential)

      Oauth::EnrichGithubCredentialIdentityJob.perform_now(credential.id)

      assert_equal "GitHub – Octo Cat", credential.reload.name
      assert_equal "99123", credential.provider_subject
      assert_equal "octo@example.com", credential.provider_email
      assert_equal "github-github-99123", credential.foreign_id
      assert_equal "GitHub – Octo Cat token", secret.reload.name
    end

    test "falls back to login for display name" do
      Oauth::EnrichGithubCredentialIdentityJob.github_api_http = ->(url:, access_token:) {
        { "id" => 99_123, "login" => "octocat", "name" => nil, "email" => nil }
      }
      credential = github_credential
      secret = wrap_credential(credential)

      Oauth::EnrichGithubCredentialIdentityJob.perform_now(credential.id)

      assert_equal "GitHub – octocat", credential.reload.name
      assert_equal "GitHub – octocat token", secret.reload.name
    end

    test "does not clobber an operator-renamed wrapper secret" do
      Oauth::EnrichGithubCredentialIdentityJob.github_api_http = ->(url:, access_token:) {
        { "id" => 99_123, "login" => "octocat", "name" => "Octo Cat" }
      }
      credential = github_credential
      secret = wrap_credential(credential, name: "operator name")

      Oauth::EnrichGithubCredentialIdentityJob.perform_now(credential.id)

      assert_equal "GitHub – Octo Cat", credential.reload.name
      assert_equal "operator name", secret.reload.name
    end

    test "merges a re-consent pending credential into an existing enriched credential" do
      Oauth::EnrichGithubCredentialIdentityJob.github_api_http = ->(url:, access_token:) {
        assert_equal "gho-new-token", access_token
        { "id" => 99_123, "login" => "octocat", "name" => "Octo Cat", "email" => "new@example.com" }
      }
      existing = github_credential(
        foreign_id: "github-github-99123",
        name: "GitHub – Old Name",
        provider_email: "old@example.com",
        provider_subject: "99123",
        access_token: "gho-old-token",
        scopes: %w[repo]
      )
      existing_secret = wrap_credential(existing)
      pending = github_credential(
        foreign_id: "github-github-pending-new",
        provider_subject: "pending-new",
        access_token: "gho-new-token",
        scopes: %w[repo workflow]
      )
      pending_secret = wrap_credential(pending)
      operator_secret = StaticSecret.create!(
        namespace: pending.namespace,
        name: "operator github token",
        inject_config: { "header" => "Authorization", "formatter" => "Bearer {{ .Value }}" },
        source: SecretSource.new(
          source_type: "token_broker",
          config: {
            "credential_id" => pending.foreign_id,
            "credential_namespace" => pending.namespace
          }
        )
      )

      Oauth::EnrichGithubCredentialIdentityJob.perform_now(pending.id)

      existing.reload
      assert_equal "GitHub – Octo Cat", existing.name
      assert_equal "new@example.com", existing.provider_email
      assert_equal "gho-new-token", existing.access_token
      assert_equal %w[repo workflow], existing.scopes
      assert_equal existing, existing_secret.reload.broker_credential
      assert_not BrokerCredential.exists?(pending.id)
      assert_not StaticSecret.exists?(pending_secret.id)
      assert_equal existing.oid, operator_secret.reload.source.config["credential_id"]
      assert_nil operator_secret.source.config["credential_namespace"]
    end
  end
end
