# frozen_string_literal: true

module Rubydex
  class Error < StandardError; end

  # Raised when `MethodAliasDefinition#target` walks an alias chain that loops back on itself.
  class AliasCycleError < Error; end
end
