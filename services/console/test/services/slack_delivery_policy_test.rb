require "test_helper"

class SlackDeliveryPolicyTest < ActiveSupport::TestCase
  test "allows the user's Slack DM, public channels, and shared private channels" do
    user = users(:acme_admin)
    user.user_identities.create!(
      provider: "slack",
      subject: "U0123456789",
      team_id: "T0123456789"
    )
    shared_private = SlackBotChannel.create!(
      team_id: "T0123456789",
      bot_user_id: "U0999999999",
      channel_id: "G1111111111",
      name: "shared-private",
      private: true,
      active: true,
      member_user_ids: [ "U0123456789", "U0999999999" ]
    )
    policy = SlackDeliveryPolicy.new(user)

    assert policy.allowed?("U0123456789")
    assert policy.allowed?("C0123456789")
    assert policy.allowed?(shared_private.channel_id)
    assert_not policy.allowed?("G9876543210")
    assert_not policy.allowed?("C9999999999")
  end

  test "allows public channels without a linked Slack identity" do
    policy = SlackDeliveryPolicy.new(users(:member_user))

    assert policy.allowed?("C0123456789")
    assert_not policy.allowed?("G9876543210")
  end

  test "resolves Slack identity from the user's connected Slack credential" do
    user = users(:member_user)
    app = oauth_apps(:acme_slack)
    app.client_secret = "slack-client-secret"
    app.update!(labels: app.labels.merge("slack_team_id" => "T0123456789"))
    BrokerCredential.create!(
      oauth_app: app,
      provider_subject: "U0123456789",
      provider_email: user.email,
      token_endpoint: app.provider_strategy.token_endpoint,
      refresh_token: "refresh-slack-delivery",
      access_token: "access-slack-delivery",
      expires_at: 1.hour.from_now,
      last_refresh: Time.current,
      external_user_key: "user-slack-delivery",
      created_by: user
    )

    policy = SlackDeliveryPolicy.new(user)

    assert_equal "U0123456789", policy.slack_user_id
    assert_equal "T0123456789", policy.slack_team_id
    assert policy.linked_identity_matches?("U0123456789", "T0123456789")
    assert policy.allowed?("C0123456789")
  end

  test "refuses ambiguous persisted Slack identities" do
    user = users(:acme_admin)
    user.user_identities.create!(provider: "slack", subject: "U0123456789", team_id: "T0123456789")
    Principal.create!(
      foreign_id: "console-user-ambiguous-delivery",
      kind: "console_user",
      console_user: user,
      slack_user_id: "U2222222222",
      slack_team_id: "T0123456789",
      created_by: user
    )

    policy = SlackDeliveryPolicy.new(user)

    assert_nil policy.slack_user_id
    assert_nil policy.slack_team_id
    assert policy.allowed?("C0123456789")
    assert_not policy.allowed?("G9876543210")
  end

  test "ignores dead or disconnected Slack credentials when resolving live identity" do
    user = users(:member_user)
    app = oauth_apps(:acme_slack)
    app.client_secret = "slack-client-secret"
    app.update!(labels: app.labels.merge("slack_team_id" => "T0123456789"))
    BrokerCredential.create!(
      oauth_app: app,
      provider_subject: "U1111111111",
      provider_email: user.email,
      token_endpoint: app.provider_strategy.token_endpoint,
      refresh_token: "refresh-disconnected",
      access_token: "access-disconnected",
      expires_at: 1.hour.from_now,
      last_refresh: Time.current,
      external_user_key: "user-slack-disconnected",
      dead: true,
      dead_reason: Oauth::Disconnect::DEAD_REASON,
      created_by: user
    )
    BrokerCredential.create!(
      oauth_app: app,
      provider_subject: "U3333333333",
      provider_email: user.email,
      token_endpoint: app.provider_strategy.token_endpoint,
      refresh_token: "refresh-invalid-grant",
      access_token: "access-invalid-grant",
      expires_at: 1.hour.from_now,
      last_refresh: Time.current,
      external_user_key: "user-slack-invalid-grant",
      dead: true,
      dead_reason: "invalid_grant",
      created_by: user
    )
    BrokerCredential.create!(
      oauth_app: app,
      provider_subject: "U4444444444",
      provider_email: user.email,
      token_endpoint: app.provider_strategy.token_endpoint,
      refresh_token: "refresh-disconnected-reason",
      access_token: "access-disconnected-reason",
      expires_at: 1.hour.from_now,
      last_refresh: Time.current,
      external_user_key: "user-slack-disconnected-reason",
      dead: false,
      dead_reason: Oauth::Disconnect::DEAD_REASON,
      created_by: user
    )
    BrokerCredential.create!(
      oauth_app: app,
      provider_subject: "U2222222222",
      provider_email: user.email,
      token_endpoint: app.provider_strategy.token_endpoint,
      refresh_token: "refresh-live",
      access_token: "access-live",
      expires_at: 1.hour.from_now,
      last_refresh: Time.current,
      external_user_key: "user-slack-live",
      created_by: user
    )

    policy = SlackDeliveryPolicy.new(user)

    assert_equal "U2222222222", policy.slack_user_id
    assert_equal "T0123456789", policy.slack_team_id
    assert policy.allowed?("U2222222222")
    assert_not policy.allowed?("U1111111111")
    assert_not policy.allowed?("U3333333333")
    assert_not policy.allowed?("U4444444444")
    assert policy.linked_identity_matches?("U2222222222", "T0123456789")
    assert_not policy.linked_identity_matches?("U1111111111", "T0123456789")
    assert_not policy.linked_identity_matches?("U3333333333", "T0123456789")
    assert_not policy.linked_identity_matches?("U4444444444", "T0123456789")
  end
end
