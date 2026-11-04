require "test_helper"

module Api
  module V1
    class ScheduledTasksControllerTest < ActionDispatch::IntegrationTest
      setup do
        @author = users(:acme_admin)
        @author.user_identities.create!(
          provider: "slack",
          subject: "U0123456789",
          team_id: "T0123456789"
        )
        @task = ScheduledTask.create!(
          name: "Incident reminder",
          prompt: "Notify when recovered",
          author: @author,
          delivery_channel: "C0123456789",
          cron_expression: "*/5 * * * *",
          enabled: true
        )
        @path = "/api/v1/scheduled_tasks/#{@task.oid}"
      end

      test "returns current task execution state without task content or writes" do
        assert_no_difference -> { Principal.count } do
          get @path, headers: auth_headers
        end

        assert_response :ok
        assert_equal "no-store", response.headers["Cache-Control"]
        assert_equal(
          {
            "id" => @task.oid,
            "enabled" => true,
            "delivery_channel" => "C0123456789"
          },
          response.parsed_body.fetch("data")
        )
        assert_not_includes response.body, @task.prompt
      end

      test "returns current disabled state" do
        @task.update!(enabled: false)

        get @path, headers: auth_headers

        assert_response :ok
        assert_equal false, response.parsed_body.dig("data", "enabled")
      end

      test "returns not found after deletion" do
        @task.destroy!

        get @path, headers: auth_headers

        assert_response :not_found
      end

      test "requires an active admin API key" do
        get @path
        assert_response :unauthorized

        get @path, headers: auth_headers(token: "iak_member-token")
        assert_response :forbidden
      end

      test "the Slack owner cancels future runs without deleting the task or returning its content" do
        original = @task.attributes
        assert_no_difference [ "ScheduledTask.count", "Principal.count" ] do
          cancel_task
        end

        assert_response :ok
        assert_equal "no-store", response.headers["Cache-Control"]
        assert_equal({ "data" => { "id" => @task.oid, "enabled" => false } }, response.parsed_body)
        assert_not_includes response.body, @task.prompt
        @task.reload
        assert_not @task.enabled?
        assert_nil @task.next_run_at
        assert_equal original.except("enabled", "next_run_at", "updated_at"),
          @task.attributes.except("enabled", "next_run_at", "updated_at")
      end

      test "repeated cancellation is idempotent" do
        cancel_task
        assert_response :ok

        assert_no_changes -> { @task.reload.attributes } do
          cancel_task
        end
        assert_response :ok
        assert_equal({ "data" => { "id" => @task.oid, "enabled" => false } }, response.parsed_body)
      end

      test "other Slack users workspaces and delivery channels cannot cancel" do
        [ { user_id: "U2222222222" }, { team_id: "T2222222222" },
          { channel_id: "C2222222222" }, { channel_id: "D0123456789" } ].each do |context|
          assert_no_changes -> { @task.reload.attributes } do
            cancel_task(**context)
          end
          assert_response :forbidden
          assert_not_includes response.body, @task.prompt
        end
      end

      test "disabled and unlinked task authors cannot cancel" do
        @author.update!(status: "disabled")
        assert_no_changes -> { @task.reload.attributes } do
          cancel_task(token: "iak_globex-ci-token")
        end
        assert_response :forbidden

        @author.update!(status: "active")
        @author.user_identities.destroy_all
        assert_no_changes -> { @task.reload.attributes } do
          cancel_task
        end
        assert_response :forbidden
      end

      test "ambiguous Slack ownership cannot cancel" do
        @author.user_identities.create!(provider: "slack", subject: "U2222222222", team_id: "T0123456789")

        assert_no_changes -> { @task.reload.attributes } do
          cancel_task
        end
        assert_response :forbidden
      end

      test "user-addressed reports can be cancelled by that owner in a DM" do
        @task.update!(delivery_channel: "U0123456789")

        assert_no_changes -> { @task.reload.attributes } do
          cancel_task
        end
        assert_response :forbidden

        cancel_task(channel_id: "D0123456789")

        assert_response :ok
        assert_not @task.reload.enabled?
        assert_nil @task.next_run_at
      end

      test "a former delivery destination does not prevent stopping the schedule" do
        SlackBotChannel.find_by!(channel_id: @task.delivery_channel).update!(archived: true)

        cancel_task

        assert_response :ok
        assert_not @task.reload.enabled?
        assert_nil @task.next_run_at
      end

      test "missing tasks return not found without changing other tasks" do
        @task.destroy!

        assert_no_difference "ScheduledTask.count" do
          cancel_task
        end
        assert_response :not_found
      end

      test "cancellation requires the existing active admin API key boundary" do
        [ [ nil, :unauthorized ], [ "iak_member-token", :forbidden ],
          [ "iak_disabled-token", :unauthorized ] ].each do |token, status|
          assert_no_changes -> { @task.reload.attributes } do
            cancel_task(token: token)
          end
          assert_response status
        end
      end

      test "malformed or caller-selected ownership fields are rejected" do
        context = { team_id: "T0123456789", user_id: "U0123456789", channel_id: "C0123456789" }
        [ context, { data: context.except(:user_id) }, { data: context.merge(user_id: [ context[:user_id] ]) },
          { data: context.merge(user_id: "U0123456789\n") },
          { data: context.merge(author_id: @author.oid) }, { data: context, principal: "operator" } ].each do |body|
          assert_no_changes -> { @task.reload.attributes } do
            post "#{@path}/cancel", params: body, headers: auth_headers, as: :json
          end
          assert_response :bad_request
        end
      end

      private

      def cancel_task(team_id: "T0123456789", user_id: "U0123456789", channel_id: "C0123456789", token: "iak_acme-ci-token")
        headers = token ? auth_headers(token: token) : {}
        post "#{@path}/cancel", params: { data: { team_id: team_id, user_id: user_id, channel_id: channel_id } },
          headers: headers, as: :json
      end

      def auth_headers(token: "iak_acme-ci-token")
        { "Authorization" => "Bearer #{token}" }
      end
    end
  end
end
