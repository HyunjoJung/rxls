#!/usr/bin/env python3
"""Type-exact equality for already parsed JSON-compatible values."""

from __future__ import annotations


def type_exact_equal(left: object, right: object) -> bool:
    """Compare recursively without Python's ``bool``/``int`` aliasing."""
    if type(left) is not type(right):
        return False
    if isinstance(left, dict):
        return (
            left.keys() == right.keys()
            and all(
                type_exact_equal(left[key], right[key])
                for key in left
            )
        )
    if isinstance(left, list):
        return len(left) == len(right) and all(
            type_exact_equal(left_item, right_item)
            for left_item, right_item in zip(left, right, strict=True)
        )
    return left == right
