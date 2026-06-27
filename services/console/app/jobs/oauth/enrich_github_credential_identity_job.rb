require "json"
require "net/http"
require "uri"

module Oauth
  class EnrichGithubCredentialIdentityJob < ApplicationJob
    queue_as :default

    USER_ENDPOINT = "https://api.github.com/user"
    class GithubProfileRetryableError < StandardError; end

    retry_on GithubProfileRetryableError, wait: :polynomially_longer, attempts: 5 do |job, error|
      credential_id = job.arguments.first
      Rails.logger.warn do
        "github oauth credential identity enrichment failed after retries: " \
          "credential_id=#{credential_id.inspect} error=#{error.class}"
      end
    end

    class << self
      attr_accessor :github_api_http
    end

    def perform(credential_id)
      credential = BrokerCredential.includes(:oauth_app, :static_secret).find_by(id: credential_id)
      return unless credential&.oauth_app&.provider == Oauth::Providers::Github::KEY
      return if credential.access_token.blank?

      profile = github_profile(credential.access_token)
      subject = profile[:subject].presence
      display_name = profile[:name].presence || profile[:email].presence || subject
      if subject.blank? || display_name.blank?
        Rails.logger.warn do
          "github oauth credential identity enrichment returned no identity: " \
            "credential=#{credential.oid}"
        end
        return
      end

      BrokerCredential.transaction do
        credential.lock!
        existing = BrokerCredential
          .where(oauth_app: credential.oauth_app, provider_subject: subject)
          .where.not(id: credential.id)
          .first

        if existing
          merge_pending_credential!(pending: credential, existing:, profile:, display_name:)
        else
          old_name = credential.name
          credential.update!(
            name: "GitHub – #{display_name}",
            provider_subject: subject,
            provider_email: profile[:email].presence || credential.provider_email,
            foreign_id: "github-#{credential.oauth_app.slug}-#{subject.downcase}"
          )
          rename_default_wrapper_secret!(credential, old_name)
        end
      end
    rescue ActiveRecord::RecordInvalid, ActiveRecord::RecordNotUnique => e
      Rails.logger.warn do
        "github oauth credential identity enrichment failed to persist: " \
          "credential=#{credential&.oid || credential_id.inspect} error=#{e.class}"
      end
    end

    private

    def merge_pending_credential!(pending:, existing:, profile:, display_name:)
      existing.lock!
      old_name = existing.name
      existing.update!(
        name: "GitHub – #{display_name}",
        provider_email: profile[:email].presence || existing.provider_email || pending.provider_email,
        access_token: pending.access_token,
        refresh_token: pending.refresh_token,
        scopes: pending.scopes,
        expires_at: pending.expires_at,
        last_refresh: pending.last_refresh,
        next_attempt_at: pending.next_attempt_at,
        failure_count: pending.failure_count,
        dead: false,
        dead_reason: nil
      )
      rename_default_wrapper_secret!(existing, old_name)
      repoint_pending_sources!(pending, existing)
      remove_pending_wrapper!(pending)
      pending.destroy!
    end

    def rename_default_wrapper_secret!(credential, old_name)
      secret = credential.static_secret
      return unless secret
      return if old_name.present? && secret.name != "#{old_name} token"

      secret.update!(name: "#{credential.name} token")
    end

    def repoint_pending_sources!(pending, existing)
      SecretSource.referencing_broker_credential(pending).find_each do |source|
        config = if source.config.is_a?(Hash)
          source.config.merge("credential_id" => existing.oid)
        else
          { "credential_id" => existing.oid }
        end
        config.delete("credential_namespace")
        source.update!(config: config)
      end
    end

    def remove_pending_wrapper!(pending)
      secret = pending.static_secret
      return unless secret

      if secret.grants.exists?
        secret.update!(broker_credential: nil)
      else
        secret.destroy!
      end
    end

    def github_profile(access_token)
      response = github_api(access_token)
      return {} unless response.is_a?(Hash)

      login = response["login"].presence
      id = response["id"]
      return {} if login.blank? || id.blank?

      {
        subject: id.to_s,
        email: response["email"].presence,
        name: response["name"].presence || login
      }
    rescue GithubProfileRetryableError
      raise
    rescue StandardError => e
      Rails.logger.debug { "github oauth profile lookup failed: #{e.class}" }
      {}
    end

    def github_api(access_token)
      return nil if access_token.blank?

      if self.class.github_api_http
        return self.class.github_api_http.call(
          url: USER_ENDPOINT,
          access_token: access_token
        )
      end

      uri = URI.parse(USER_ENDPOINT)
      req = Net::HTTP::Get.new(uri)
      req["Accept"] = "application/vnd.github+json"
      req["Authorization"] = "Bearer #{access_token}"
      req["X-GitHub-Api-Version"] = "2022-11-28"
      req["User-Agent"] = "centaur-console"

      http = Net::HTTP.new(uri.host, uri.port)
      http.use_ssl = uri.scheme == "https"
      http.open_timeout = 5
      http.read_timeout = 5

      response = http.request(req)
      status = response.code.to_i
      if status == 429 || status >= 500
        raise GithubProfileRetryableError, "github user lookup http #{status}"
      end
      unless status / 100 == 2
        Rails.logger.warn { "github oauth profile lookup failed: status=#{status}" }
        return nil
      end

      parsed = JSON.parse(response.body.to_s)
      parsed.is_a?(Hash) ? parsed : nil
    rescue GithubProfileRetryableError
      raise
    rescue JSON::ParserError => e
      Rails.logger.warn { "github oauth profile lookup returned invalid JSON: #{e.class}" }
      nil
    rescue StandardError => e
      raise GithubProfileRetryableError, e.class.name
    end
  end
end
