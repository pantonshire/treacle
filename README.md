# `treacle`
A generic event debouncer for Rust, for grouping events which occur close together in time into a
single event.

For example, you may have some
[templates](https://www.arewewebyet.org/topics/templating/)
which you want to reload whenever a template file changes. However, if many small changes are made
to the template files in quick succession, it would be wasteful to reload the templates for every
change; instead, a debouncer could be used to group the changes that occur at a similar time into
a single change, so the templates are only reloaded once.

## Usage

