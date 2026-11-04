module Api
  module V1
    class ScheduledTasksController < Api::BaseController
      wrap_parameters false

      def show
        response.headers["Cache-Control"] = "no-store"
        task = ScheduledTask.find_by_oid!(params[:id])
        render json: {
          data: {
            id: task.oid,
            enabled: task.enabled?,
            delivery_channel: task.delivery_channel
          }
        }
      end

      def cancel
        response.headers["Cache-Control"] = "no-store"
        context = cancellation_context
        task = ScheduledTask.find_by_oid!(params[:id])
        task.cancel_from_slack!(**context.symbolize_keys)
        render json: { data: { id: task.oid, enabled: false } }
      rescue ScheduledTask::SlackCancellationNotAllowed
        render_error(status: :forbidden, message: "scheduled task cancellation is not allowed")
      end

      private

      def cancellation_context
        body = request.request_parameters
        context = body["data"]
        formats = {
          "team_id" => Principal::SLACK_TEAM_ID_FORMAT,
          "user_id" => Principal::SLACK_USER_ID_FORMAT,
          "channel_id" => Principal::SLACK_CHANNEL_ID_FORMAT
        }
        unless body.keys == [ "data" ] && context.is_a?(Hash) && context.keys.sort == formats.keys.sort &&
            formats.all? { |key, format| context[key].is_a?(String) && format.match?(context[key]) }
          raise ActionController::BadRequest, "invalid Slack cancellation context"
        end
        context
      end
    end
  end
end
