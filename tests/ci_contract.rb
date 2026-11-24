require 'yaml'

root = File.expand_path('..', __dir__)
workflow = YAML.safe_load(File.read(File.join(root, '.github/workflows/packages.yml')))
jobs = workflow.fetch('jobs')
raise 'expected four platforms' unless jobs.keys.sort == %w[android linux macos windows]
arch = jobs.fetch('linux').dig('strategy', 'matrix', 'include').map { |entry| entry.fetch('arch') }
raise 'missing Linux architecture' unless arch.sort == %w[aarch64 x86_64]
jobs.each do |platform, job|
  uploads = job.fetch('steps').select { |step| step['uses'].to_s.start_with?('actions/upload-artifact@') }
  raise "missing package for #{platform}" unless uploads.size == 1 && uploads.first.dig('with', 'if-no-files-found') == 'error'
end
raise 'expected unsigned APK for offline signing' unless jobs.fetch('android').fetch('steps').last.dig('with', 'path') == 'target/apk/aligned.apk'
puts 'CI contract: OK'
