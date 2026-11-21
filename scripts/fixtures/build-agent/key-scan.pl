# Looks for byte strings in everything a container holds: every file, and
# every process's environment, command line and memory. Part of
# scripts/test-build-agent.sh, which runs it in a build container, as root
# with every capability, from outside the build (`docker exec --privileged`)
# while the build has its SSH agent.
#
# Standard input: one line per string, `<name> <hex>`. The strings are the
# worker's SSH key in the forms it could have leaked in, and a canary the
# build is known to hold, which shows the looking finds what is there. They
# come on standard input so that they are in this process's memory and
# nowhere else in the container; this process leaves itself out.
#
# Standard output: `scan <name> files=N environ=N cmdline=N memory=N` for
# each string, then `scan processes=N unread=N skipped=N`: how many processes
# were looked at, how many had memory that could not be opened, and how many
# mappings were too large to read.
use strict;
use File::Find;

my $CHUNK = 4 << 20;
my $LARGEST = 256 << 20;

my (@names, %needle, %found);
while (my $line = <STDIN>) {
    my ($name, $hex) = split ' ', $line;
    next unless defined $hex;
    push @names, $name;
    $needle{$name} = pack 'H*', $hex;
    $found{$name} = { files => 0, environ => 0, cmdline => 0, memory => 0 };
}
my $overlap = 0;
for my $name (@names) {
    my $len = length $needle{$name};
    $overlap = $len - 1 if $len - 1 > $overlap;
}

# Which strings are in `$text`.
sub within {
    my ($text) = @_;
    return grep { index($$text, $needle{$_}) >= 0 } @names;
}

# Count each string once for what `$read` yields, a chunk at a time, with the
# end of each chunk kept so that a string across two is seen.
sub look {
    my ($where, $read) = @_;
    my ($tail, %seen) = ('');
    while (defined(my $chunk = $read->())) {
        my $text = $tail . $chunk;
        $seen{$_} = 1 for within(\$text);
        $tail = length($text) > $overlap ? substr($text, -$overlap) : $text;
    }
    $found{$_}{$where}++ for keys %seen;
}

sub whole {
    my ($where, $path) = @_;
    open(my $file, '<:raw', $path) or return;
    look($where, sub {
        my $got = sysread($file, my $chunk, $CHUNK);
        return $got ? $chunk : undef;
    });
    close $file;
}

my ($processes, $unread, $skipped) = (0, 0, 0);
for my $dir (glob '/proc/[0-9]*') {
    my ($pid) = $dir =~ /(\d+)$/;
    next if $pid == $$;
    $processes++;
    whole('environ', "$dir/environ");
    whole('cmdline', "$dir/cmdline");
    my $opened = open(my $maps, '<', "$dir/maps") && open(my $mem, '<:raw', "$dir/mem");
    if (!$opened) {
        $unread++;
        next;
    }
    my @regions;
    while (my $line = <$maps>) {
        my ($from, $to, $mode) = $line =~ /^([0-9a-f]+)-([0-9a-f]+) (\S+)/ or next;
        next unless $mode =~ /^r/;
        next if $line =~ /\[(?:vvar|vvar_vclock|vsyscall)\]/;
        push @regions, [hex $from, hex $to];
    }
    close $maps;
    my %seen;
    for my $region (@regions) {
        my ($at, $end) = @$region;
        if ($end - $at > $LARGEST) {
            $skipped++;
            next;
        }
        my $tail = '';
        while ($at < $end) {
            my $want = $end - $at > $CHUNK ? $CHUNK : $end - $at;
            sysseek($mem, $at, 0) or last;
            my $got = sysread($mem, my $chunk, $want);
            last unless $got;
            my $text = $tail . $chunk;
            $seen{$_} = 1 for within(\$text);
            $tail = length($text) > $overlap ? substr($text, -$overlap) : $text;
            $at += $got;
        }
    }
    close $mem;
    $found{$_}{memory}++ for keys %seen;
}

find(
    {
        no_chdir => 1,
        wanted   => sub {
            if ($File::Find::name =~ m{^/(?:proc|sys|dev)$}) {
                $File::Find::prune = 1;
                return;
            }
            whole('files', $File::Find::name) if -f $File::Find::name && !-l $File::Find::name;
        },
    },
    '/'
);

for my $name (@names) {
    my $at = $found{$name};
    print "scan $name files=$at->{files} environ=$at->{environ} cmdline=$at->{cmdline} memory=$at->{memory}\n";
}
print "scan processes=$processes unread=$unread skipped=$skipped\n";
