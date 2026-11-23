require "test_helper"

# A queue that no worker claims holds its jobs forever.
class SolidQueueConfigurationTest < ActiveSupport::TestCase
  test "production workers claim every recurring task's queue" do
    tasks = config_for("recurring")
    assert_includes tasks.keys, "clear_solid_queue_finished_jobs"

    tasks.each do |key, task|
      queue = task["queue"].presence&.to_s || job_class(task).new.queue_name
      assert workers_claiming(queue).any?,
        "recurring task #{key} enqueues onto #{queue}, which no production worker claims"
    end
  end

  test "one dedicated thread runs the recurring cleanups" do
    queue = SolidQueue::RecurringJob.new.queue_name
    workers = workers_claiming(queue)
    assert_equal 1, workers.size, "#{queue} must have exactly one worker"
    worker = workers.first
    assert_equal [ queue ], Array(worker.fetch("queues")), "#{queue} must not share a worker with other queues"
    assert_equal 1, worker.fetch("threads", 3)
    assert_equal 1, worker.fetch("processes", 1)
  end

  private
    def config_for(name)
      ActiveSupport::ConfigurationFile.parse(Rails.root.join("config/#{name}.yml")).fetch("production")
    end

    # A worker without queues claims all of them.
    def workers_claiming(queue)
      config_for("queue").fetch("workers").select do |worker|
        Array(worker.fetch("queues", "*")).any? { |pattern| claims?(pattern.to_s, queue) }
      end
    end

    # Tasks with a command and no class run as SolidQueue::RecurringJob.
    def job_class(task)
      task["class"].presence&.constantize || SolidQueue::RecurringJob
    end

    def claims?(pattern, queue)
      pattern == "*" || pattern == queue || (pattern.end_with?("*") && queue.start_with?(pattern.delete_suffix("*")))
    end
end
