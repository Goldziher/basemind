from dataclasses import dataclass
from typing import Generic, TypeVar
import typing

T = TypeVar("T")

type UserId = int
type Pair[K] = tuple[K, K]

registry: dict[str, int]


@dataclass
class User:
    name: str
    age: int = 0


class Box(Generic[T]):
    item: T


class Holder(typing.Generic[T]):
    pass


def build() -> None:
    local: int
    local = 1
