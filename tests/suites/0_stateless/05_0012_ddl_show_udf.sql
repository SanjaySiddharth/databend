SHOW FUNCTION isnotempty; -- {ErrorCode 4071}

CREATE FUNCTION isnotempty AS not(isnull(@0)) DESC AS 'This is a description';
SHOW FUNCTION isnotempty;
