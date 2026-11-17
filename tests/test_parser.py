from zora.parser import CommandParser


def test_delimited_command():
    p = CommandParser()
    assert p.feed("ignore this Hey Zora create a branch") == []
    assert p.feed("and add tests. Okay Zora do it") == ["create a branch and add tests"]


def test_ignores_ambient_speech():
    p = CommandParser()
    assert p.feed("Please refactor the service") == []
    assert p.feed("Zora do it") == []


def test_multiple_commands_and_trailing_text():
    p = CommandParser()
    assert p.feed("Hey Zora fix bug Zora do it. Hey Zora run tests Zora do it") == [
        "fix bug", "run tests"
    ]


def test_split_activation():
    p = CommandParser()
    p.feed("I said hey")
    assert p.feed("Zora check the logs") == []
    assert p.feed("Zora do it") == ["check the logs"]


def test_discards_oversized_command():
    p = CommandParser(max_chars=40)
    p.feed("Hey Zora")
    p.feed("x" * 41)
    assert not p.active
    assert p.feed("Zora do it") == []
