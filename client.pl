#!/usr/bin/env perl

use v5.20;

use warnings;
use warnings qw< FATAL utf8>;
use autodie;
use utf8;
use feature qw< unicode_strings >;

use Encode;
use Socket;

binmode(STDOUT, ":utf8");
binmode(STDIN, ":utf8");
use constant SOCKET_PATH => "test.sock";

socket(my $sock, PF_UNIX, SOCK_STREAM, 0) or die "socket: $!";
connect($sock, pack_sockaddr_un(SOCKET_PATH)) or die "connect: $!";

sub ready {
	send($sock, encode("UTF-8", "ready\n"), 0);
	recv($sock, my $reply, 512, 0);
	chomp($reply);
	unless ($reply =~ "^ok") { die "Ready state unknown"}
}

sub slash {
	my ($command) = @_;
	$command .= "\n" unless ($command =~ /\n$/);
	
	send($sock, encode("UTF-8", "slash $command"), 0);
	recv($sock, my $reply, 512, 0);
	chomp($reply);
	unless ($reply =~ "ok") { die "Slash command failed" }
}

sub get_line {
	my ($re, $command) = @_;
	$command .= "\n" unless ($command =~ /\n$/);
	
	send($sock, encode("UTF-8", "get-line =#$re#= $command"), 0);
	recv($sock, my $reply, 512, 0);
	chomp($reply);
	unless ($reply =~ /^ok/) { die "get-line failed" }
	return $reply =~ s/^ok: get-line, //r;
}

use Data::Dumper;

ready();

sleep 2;

get_line("^There are .* players online", "list");

while (1) {
	my $tps_line = get_line("TPS from last", "tps");
	say "got: $tps_line";
	$tps_line =~ s/\e\[.*?m//g;
	$tps_line =~ /:\  ([0-9.]+) \,\  ([0-9.]+) \,\  ([0-9.]+)/x;
	
	say "TPS: 1m = $1, 5m = $2, 15m = $3";
	sleep 5;
}
