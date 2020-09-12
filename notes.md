# Notes

## Status

Now, call crate::console in crate::main and test what happens when some initialization messages fail

## Important

- Input filters can create deadlocks if they are not allowed to expire.
  eg. filterA block "b" waits for "a". filterB blocks "a" and waits for "b".
  A solution is to always match the command you expect: ie. filterA should also block "a",
  but it will be allowed because it's from the same client.
  This doesn't solve the problem if a filter is renewed indefinitely. So don't do that.

- I have no idea when a type needs to be Pin, Unpin, Send or Sync. They're usually added because the compiler complains
  about async functions not being Send. 

- `SharedOutputFilters` is a Vec instad of a HashSet/HashMap because that's faster.
  But `SharedInputFilters` is a HashMap because we need to remove our filter.

## TODO

- Add console history for clients
- Send all output to a function because we're handling user input
- Handle error gracefully when the server command is not found

## Scratchpad

scripting languages:
- Python
- Perl
- Raku
